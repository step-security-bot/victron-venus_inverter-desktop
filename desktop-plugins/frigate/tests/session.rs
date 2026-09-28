use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

struct Worker {
    child: Child,
    input: Option<ChildStdin>,
    frames: mpsc::Receiver<Value>,
}

impl Worker {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_inverter-frigate-worker"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let output = child.stdout.take().unwrap();
        let (sender, frames) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else {
                    break;
                };
                assert!(line.len() < 65536);
                let value = serde_json::from_str(&line).expect("stdout must contain protocol only");
                if sender.send(value).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            frames,
        }
    }

    fn send(&mut self, frame: Value) {
        writeln!(self.input.as_mut().unwrap(), "{frame}").unwrap();
        self.input.as_mut().unwrap().flush().unwrap();
    }

    fn hello(&mut self) {
        self.hello_with_api("1.8.0");
    }

    fn hello_with_api(&mut self, api: &str) {
        self.send(json!({"type":"hello","protocol_version":1,"host_api_version":api,"plugin_id":"inverter-desktop.frigate"}));
        assert_eq!(
            self.frame(),
            json!({"type":"ready","protocol_version":1,"host_api_version":api,"plugin_id":"inverter-desktop.frigate"})
        );
    }

    fn configure(&mut self, port: u16) {
        self.configure_frame(configuration(port));
    }

    fn configure_frame(&mut self, frame: Value) {
        self.send(frame);
        assert_eq!(
            self.frame(),
            json!({"type":"configuration_ready","revision":"fixture-1"})
        );
        self.status("Connecting");
    }

    fn frame(&self) -> Value {
        self.frames
            .recv_timeout(Duration::from_secs(5))
            .expect("worker response")
    }
    fn status(&self, value: &str) {
        assert_eq!(
            self.frame(),
            json!({
                "type":"contributions",
                "items":[{"kind":"status","id":"connection","title":"Frigate MQTT","value":value,"tone":match value { "Connected"=>"success","Disconnected"=>"warning",_=>"neutral" }}],
                "presentation":[{"kind":"connection","id":"frigate-mqtt","title":"Frigate MQTT","connected":value=="Connected"}]
            })
        );
    }
    fn exit(&mut self, success: bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert_eq!(status.success(), success);
                return;
            }
            assert!(Instant::now() < deadline, "worker must stop promptly");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn shutdown(&mut self) {
        self.send(json!({"type":"shutdown"}));
        self.exit(true);
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn configuration(port: u16) -> Value {
    json!({"type":"configuration","configuration":{"revision":"fixture-1","values":{"mqtt_host":"127.0.0.1","mqtt_port":port},"secrets":{"mqtt_username":"fixture-user","mqtt_password":"fixture-password"}}})
}

fn listener() -> TcpListener {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    listener
}

fn accept(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "worker must connect");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("listener failed: {error}"),
        }
    }
}

fn packet(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut header = [0];
    stream.read_exact(&mut header).unwrap();
    let mut length = 0;
    let mut multiplier = 1;
    loop {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        length += (byte[0] & 127) as usize * multiplier;
        if byte[0] & 128 == 0 {
            break;
        }
        multiplier *= 128;
        assert!(multiplier <= 128 * 128 * 128);
    }
    assert!(length <= 32 * 1024);
    let mut data = vec![0; length];
    stream.read_exact(&mut data).unwrap();
    (header[0], data)
}

fn mqtt_string<'a>(bytes: &mut &'a [u8]) -> &'a [u8] {
    let length = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
    let value = &bytes[2..2 + length];
    *bytes = &bytes[2 + length..];
    value
}

fn subscribe(listener: &TcpListener) -> TcpStream {
    let mut stream = accept(listener);
    let (header, connect) = packet(&mut stream);
    assert_eq!(header, 0x10);
    let mut bytes = connect.as_slice();
    assert_eq!(mqtt_string(&mut bytes), b"MQTT");
    assert_eq!(bytes[0], 4);
    assert_eq!(bytes[1], 0xc2, "clean session, username and password only");
    bytes = &bytes[4..];
    assert!(mqtt_string(&mut bytes).starts_with(b"inverter-frigate-"));
    assert_eq!(mqtt_string(&mut bytes), b"fixture-user");
    assert_eq!(mqtt_string(&mut bytes), b"fixture-password");
    assert!(bytes.is_empty());
    stream.write_all(&[0x20, 2, 0, 0]).unwrap();
    let (header, subscribe) = packet(&mut stream);
    assert_eq!(header, 0x82);
    let mut bytes = &subscribe[2..];
    assert_eq!(mqtt_string(&mut bytes), b"frigate/events");
    assert_eq!(bytes, [0]);
    stream
        .write_all(&[0x90, 3, subscribe[0], subscribe[1], 0])
        .unwrap();
    stream
}

fn publish(stream: &mut TcpStream, id: &str, camera: &str, retain: bool) {
    publish_event(
        stream,
        json!({"type":"new","after":{"id":id,"camera":camera}}),
        retain,
    );
}

fn completed(id: &str, camera: &str) -> Value {
    json!({"type":"end","after":{"id":id,"camera":camera,"has_clip":true,"start_time":1}})
}

fn publish_event(stream: &mut TcpStream, event: Value, retain: bool) {
    let topic = b"frigate/events";
    let mut payload = Vec::new();
    payload.extend_from_slice(&(topic.len() as u16).to_be_bytes());
    payload.extend_from_slice(topic);
    payload.extend_from_slice(&serde_json::to_vec(&event).unwrap());
    let mut bytes = vec![if retain { 0x31 } else { 0x30 }];
    let mut length = payload.len();
    loop {
        let mut byte = (length % 128) as u8;
        length /= 128;
        if length > 0 {
            byte |= 128;
        }
        bytes.push(byte);
        if length == 0 {
            break;
        }
    }
    bytes.extend_from_slice(&payload);
    stream.write_all(&bytes).unwrap();
}

#[test]
fn live_start_requests_one_scoped_host_preview_and_end_never_opens_a_clip() {
    let broker = listener();
    let http = listener();
    let mut worker = Worker::start();
    worker.hello_with_api("1.8.0");
    let mut frame = configuration(broker.local_addr().unwrap().port());
    let base = format!(
        "http://127.0.0.1:{}/prefix",
        http.local_addr().unwrap().port()
    );
    frame["configuration"]["values"]["frigate_base_url"] = json!(base);
    worker.configure_frame(frame);
    let mut stream = subscribe(&broker);
    worker.status("Connected");
    publish(&mut stream, "event ?#é", "é space?#", true);
    publish(&mut stream, "event ?#é", "é space?#", false);
    let live = worker.frame();
    assert_eq!(live["type"], "http_live");
    assert_eq!(live["title"], "Frigate É space?# camera motion detected");
    assert_eq!(
        live["url"],
        format!("{base}/api/%C3%A9%20space%3F%23?fps=2&height=360")
    );
    assert!(live.get("body").is_none() && live.get("duration_seconds").is_none());
    publish_event(&mut stream, completed("event ?#é", "front"), true);
    publish_event(&mut stream, completed("event ?#é", "front"), false);
    publish(&mut stream, "event ?#é", "other", false);
    publish(&mut stream, "sibling", "é space?#", false);
    publish_event(&mut stream, completed("event ?#é", "other"), false);
    publish_event(&mut stream, completed("sibling", "front"), false);
    publish_event(&mut stream, completed("different", "back"), false);
    assert!(worker
        .frames
        .recv_timeout(Duration::from_millis(100))
        .is_err());
    assert!(matches!(http.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock));
    worker.shutdown();
}

#[test]
fn excluded_cameras_never_emit_live_notification_or_legacy_clip_frames() {
    for (api, with_base, expected) in [
        ("1.8.0", true, "http_live"),
        ("1.8.0", false, "notification"),
    ] {
        let broker = listener();
        let mut worker = Worker::start();
        worker.hello_with_api(api);
        let mut config = configuration(broker.local_addr().unwrap().port());
        config["configuration"]["values"]["excluded_cameras"] = json!(r#"["front"]"#);
        if with_base {
            config["configuration"]["values"]["frigate_base_url"] =
                json!("https://frigate.invalid");
        }
        worker.configure_frame(config);
        let mut stream = subscribe(&broker);
        worker.status("Connected");
        for i in 0..40 {
            let id = format!("excluded-{i}");
            publish(&mut stream, &id, "front", false);
            publish_event(&mut stream, completed(&id, "front"), false);
        }
        // Excluded bursts must not consume notification or deduplication budgets.
        publish(&mut stream, "excluded-0", "back", false);
        let admitted = worker.frame();
        assert_eq!(admitted["type"], expected);
        assert_eq!(admitted["title"], "Frigate Back camera motion detected");
        assert!(worker
            .frames
            .recv_timeout(Duration::from_millis(100))
            .is_err());
        drop(stream);
        worker.status("Disconnected");
        worker.status("Connecting");
        let mut stream = subscribe(&broker);
        worker.status("Connected");
        publish(&mut stream, "after-reconnect", "front", false);
        publish(&mut stream, "included-after-reconnect", "side", false);
        assert_eq!(
            worker.frame()["title"],
            "Frigate Side camera motion detected"
        );
        assert!(worker
            .frames
            .recv_timeout(Duration::from_millis(100))
            .is_err());
        worker.shutdown();
    }
}

#[test]
fn missing_base_keeps_the_session_motion_only() {
    for (api, base) in [("1.8.0", None), ("1.8.0", Some(""))] {
        let broker = listener();
        let mut worker = Worker::start();
        worker.hello_with_api(api);
        let mut frame = configuration(broker.local_addr().unwrap().port());
        if let Some(base) = base {
            frame["configuration"]["values"]["frigate_base_url"] = json!(base);
        }
        worker.configure_frame(frame);
        let mut stream = subscribe(&broker);
        worker.status("Connected");
        publish_event(&mut stream, completed("event", "front"), false);
        publish(&mut stream, "event", "front", false);
        assert_eq!(worker.frame()["type"], "notification");
        assert!(worker
            .frames
            .recv_timeout(Duration::from_millis(100))
            .is_err());
        worker.shutdown();
    }
}

#[test]
fn live_motion_history_survives_broker_reconnect() {
    let broker = listener();
    let mut worker = Worker::start();
    worker.hello_with_api("1.8.0");
    let mut frame = configuration(broker.local_addr().unwrap().port());
    frame["configuration"]["values"]["frigate_base_url"] = json!("http://frigate.local");
    worker.configure_frame(frame);
    let mut first = subscribe(&broker);
    worker.status("Connected");
    publish(&mut first, "original", "front", false);
    assert_eq!(worker.frame()["type"], "http_live");
    drop(first);
    worker.status("Disconnected");
    worker.status("Connecting");
    let mut second = subscribe(&broker);
    worker.status("Connected");
    publish(&mut second, "original", "other", false);
    publish(&mut second, "sibling", "front", false);
    publish_event(&mut second, completed("original", "front"), false);
    publish(&mut second, "new", "back", false);
    let live = worker.frame();
    assert_eq!(live["type"], "http_live");
    assert_eq!(live["title"], "Frigate Back camera motion detected");
    assert!(worker
        .frames
        .recv_timeout(Duration::from_millis(100))
        .is_err());
    worker.shutdown();
}

#[test]
fn invalid_media_configuration_fails_before_network_and_never_echoes_url_credentials() {
    for base in [
        "https://user:private-token@frigate.local",
        "http://frigate.local?token=private",
        "http://frigate.local/a/../b",
        "http://frigate.local/a%2fb",
    ] {
        let broker = listener();
        let mut worker = Worker::start();
        worker.hello_with_api("1.8.0");
        let mut frame = configuration(broker.local_addr().unwrap().port());
        frame["configuration"]["values"]["frigate_base_url"] = json!(base);
        worker.send(frame);
        worker.exit(false);
        let mut errors = String::new();
        worker
            .child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut errors)
            .unwrap();
        assert_eq!(errors, "Frigate worker session failed\n");
        assert!(worker
            .frames
            .recv_timeout(Duration::from_millis(100))
            .is_err());
        assert!(
            matches!(broker.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock)
        );
    }
}

#[test]
fn network_starts_after_configuration_and_only_subscribes_to_configured_topic() {
    let listener = listener();
    let mut worker = Worker::start();
    worker.hello();
    assert!(matches!(listener.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock));
    worker.configure(listener.local_addr().unwrap().port());
    let mut stream = subscribe(&listener);
    worker.status("Connected");
    publish(&mut stream, "retained-event", "front", true);
    publish(&mut stream, "retained-event", "front", false);
    let first = worker.frame();
    assert_eq!(first["type"], "notification");
    assert_eq!(first["title"], "Frigate Front camera motion detected");
    assert_eq!(first["body"], "Motion started");
    publish(&mut stream, "retained-event", "other", false);
    publish(&mut stream, "same-camera-event", "front", false);
    publish(&mut stream, "other-event", "back", false);
    let second = worker.frame();
    assert_eq!(second["title"], "Frigate Back camera motion detected");
    assert_ne!(first["id"], second["id"]);
    assert!(worker
        .frames
        .recv_timeout(Duration::from_millis(100))
        .is_err());
    worker.shutdown();
    let mut byte = [0];
    assert_eq!(
        stream.read(&mut byte).unwrap(),
        0,
        "shutdown closes MQTT socket"
    );
}

#[test]
fn reconnect_resubscribes_and_preserves_event_deduplication() {
    let listener = listener();
    let mut worker = Worker::start();
    worker.hello();
    worker.configure(listener.local_addr().unwrap().port());
    let mut first = subscribe(&listener);
    worker.status("Connected");
    publish(&mut first, "original", "front", false);
    assert_eq!(worker.frame()["type"], "notification");
    drop(first);
    worker.status("Disconnected");
    worker.status("Connecting");
    let mut second = subscribe(&listener);
    worker.status("Connected");
    publish(&mut second, "original", "different", false);
    publish(&mut second, "new", "back", false);
    assert_eq!(
        worker.frame()["title"],
        "Frigate Back camera motion detected"
    );
    assert!(worker
        .frames
        .recv_timeout(Duration::from_millis(100))
        .is_err());
    worker.shutdown();
}

#[test]
fn eof_cancels_connection_wait_and_configuration_errors_do_not_leak_credentials() {
    let listener = listener();
    let mut worker = Worker::start();
    worker.hello();
    worker.configure(listener.local_addr().unwrap().port());
    let mut stream = accept(&listener);
    let _ = packet(&mut stream);
    worker.input.take();
    worker.exit(true);
    let mut byte = [0];
    assert_eq!(stream.read(&mut byte).unwrap(), 0);
    let mut invalid = Worker::start();
    invalid.hello();
    let mut frame = configuration(listener.local_addr().unwrap().port());
    frame["configuration"]["values"]["mqtt_topic"] = json!("#");
    invalid.send(frame);
    invalid.exit(false);
    let mut errors = String::new();
    invalid
        .child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut errors)
        .unwrap();
    assert_eq!(errors, "Frigate worker session failed\n");
    assert!(matches!(listener.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock));
}

#[test]
fn tls_never_falls_back_to_plaintext_mqtt() {
    let listener = listener();
    let mut worker = Worker::start();
    worker.hello();
    let mut frame = configuration(listener.local_addr().unwrap().port());
    frame["configuration"]["values"]["mqtt_tls"] = json!(true);
    worker.send(frame);
    assert_eq!(worker.frame()["type"], "configuration_ready");
    worker.status("Connecting");
    let mut stream = accept(&listener);
    let mut header = [0; 3];
    stream.read_exact(&mut header).unwrap();
    assert_eq!(header[0], 0x16, "TLS handshake, never MQTT CONNECT");
    assert_eq!(header[1], 3);
    stream.write_all(&[0x20, 2, 0, 0]).unwrap();
    drop(stream);
    worker.status("Disconnected");
    worker.shutdown();
}

fn blocked_output_session(eof: bool) {
    struct Guard(Child);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let listener = listener();
    let mut worker = Guard(
        Command::new(env!("CARGO_BIN_EXE_inverter-frigate-worker"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    // A valid large semantic-version build identifier produces a full-size
    // Ready frame. Retain the read handle without draining it, then queue the
    // configuration: its acknowledgement cannot fit in the occupied pipe.
    let mut hello = json!({"type":"hello","protocol_version":1,"host_api_version":"1.8.0+","plugin_id":"inverter-desktop.frigate"});
    let padding = 65536 - serde_json::to_vec(&hello).unwrap().len() - 1;
    hello["host_api_version"] = json!(format!("1.8.0+{}", "a".repeat(padding)));
    let input = worker.0.stdin.as_mut().unwrap();
    writeln!(input, "{hello}").unwrap();
    writeln!(
        input,
        "{}",
        configuration(listener.local_addr().unwrap().port())
    )
    .unwrap();
    input.flush().unwrap();
    std::thread::sleep(Duration::from_millis(150));
    assert!(worker.0.try_wait().unwrap().is_none());
    assert!(matches!(listener.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock));
    if eof {
        worker.0.stdin.take();
    } else {
        writeln!(
            worker.0.stdin.as_mut().unwrap(),
            "{}",
            json!({"type":"shutdown"})
        )
        .unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = worker.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "stdout must not delay shutdown");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn shutdown_does_not_wait_for_backpressured_stdout() {
    blocked_output_session(false);
}

#[test]
fn eof_does_not_wait_for_backpressured_stdout() {
    blocked_output_session(true);
}
