use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

pub struct Worker {
    pub child: Child,
    pub input: Option<ChildStdin>,
    pub frames: mpsc::Receiver<Value>,
    provider: String,
    title: String,
}

impl Worker {
    pub fn start(binary: &str, provider: &str, title: &str) -> Self {
        let mut child = Command::new(binary)
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
            provider: provider.into(),
            title: title.into(),
        }
    }

    pub fn send(&mut self, frame: Value) {
        writeln!(self.input.as_mut().unwrap(), "{frame}").unwrap();
        self.input.as_mut().unwrap().flush().unwrap();
    }

    pub fn hello(&mut self) {
        self.hello_with_api(if self.provider == "kerberos" {
            "1.9.0"
        } else {
            "1.8.0"
        });
    }

    pub fn hello_with_api(&mut self, api: &str) {
        self.send(json!({"type":"hello","protocol_version":1,"host_api_version":api,"plugin_id":format!("inverter-desktop.{}",self.provider)}));
        assert_eq!(
            self.frame(),
            json!({"type":"ready","protocol_version":1,"host_api_version":api,"plugin_id":format!("inverter-desktop.{}",self.provider)})
        );
    }

    pub fn configure(&mut self, port: u16) {
        self.configure_frame(configuration(port));
    }

    pub fn configure_frame(&mut self, frame: Value) {
        self.send(frame);
        assert_eq!(
            self.frame(),
            json!({"type":"configuration_ready","revision":"fixture-1"})
        );
        self.status("Connecting");
    }

    pub fn frame(&self) -> Value {
        self.frames
            .recv_timeout(Duration::from_secs(5))
            .expect("worker response")
    }
    pub fn status(&self, value: &str) {
        let title = format!("{} MQTT", self.title);
        assert_eq!(
            self.frame(),
            json!({
                "type":"contributions",
                "items":[{"kind":"status","id":"connection","title":title,"value":value,"tone":match value { "Connected"=>"success","Disconnected"=>"warning",_=>"neutral" }}],
                "presentation":[{"kind":"connection","id":format!("{}-mqtt",self.provider),"title":title,"connected":value=="Connected"}]
            })
        );
    }
    pub fn exit(&mut self, success: bool) {
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
    pub fn shutdown(&mut self) {
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

pub fn configuration(port: u16) -> Value {
    json!({"type":"configuration","configuration":{"revision":"fixture-1","values":{"mqtt_host":"127.0.0.1","mqtt_port":port},"secrets":{"mqtt_username":"fixture-user","mqtt_password":"fixture-password"}}})
}

pub fn listener() -> TcpListener {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    listener
}

pub fn accept(listener: &TcpListener) -> TcpStream {
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

pub fn packet(stream: &mut TcpStream) -> (u8, Vec<u8>) {
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

pub fn begin_subscribe(
    listener: &TcpListener,
    provider: &str,
    topics: &[&str],
) -> (TcpStream, [u8; 2]) {
    let mut stream = accept(listener);
    let (header, connect) = packet(&mut stream);
    assert_eq!(header, 0x10);
    let mut bytes = connect.as_slice();
    assert_eq!(mqtt_string(&mut bytes), b"MQTT");
    assert_eq!(bytes[0], 4);
    assert_eq!(bytes[1], 0xc2, "clean session, username and password only");
    bytes = &bytes[4..];
    assert!(mqtt_string(&mut bytes).starts_with(format!("inverter-{provider}-").as_bytes()));
    assert_eq!(mqtt_string(&mut bytes), b"fixture-user");
    assert_eq!(mqtt_string(&mut bytes), b"fixture-password");
    assert!(bytes.is_empty());
    stream.write_all(&[0x20, 2, 0, 0]).unwrap();
    let (header, subscribe) = packet(&mut stream);
    assert_eq!(header, 0x82);
    let mut bytes = &subscribe[2..];
    for topic in topics {
        assert_eq!(mqtt_string(&mut bytes), topic.as_bytes());
        assert_eq!(bytes[0], 0);
        bytes = &bytes[1..];
    }
    assert!(bytes.is_empty());
    (stream, [subscribe[0], subscribe[1]])
}

pub fn subscribe(listener: &TcpListener, provider: &str, topics: &[&str]) -> TcpStream {
    let (mut stream, pkid) = begin_subscribe(listener, provider, topics);
    let mut ack = vec![0x90, (2 + topics.len()) as u8, pkid[0], pkid[1]];
    ack.resize(4 + topics.len(), 0);
    stream.write_all(&ack).unwrap();
    stream
}

pub fn publish(stream: &mut TcpStream, topic: &str, message: &[u8], retained: bool) {
    let mut payload = Vec::new();
    payload.extend_from_slice(&(topic.len() as u16).to_be_bytes());
    payload.extend_from_slice(topic.as_bytes());
    payload.extend_from_slice(message);
    let mut packet = vec![if retained { 0x31 } else { 0x30 }];
    let mut length = payload.len();
    loop {
        let mut byte = (length % 128) as u8;
        length /= 128;
        if length > 0 {
            byte |= 128;
        }
        packet.push(byte);
        if length == 0 {
            break;
        }
    }
    packet.extend_from_slice(&payload);
    stream.write_all(&packet).unwrap();
}
