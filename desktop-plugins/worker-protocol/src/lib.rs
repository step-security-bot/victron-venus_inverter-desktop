//! Minimal desktop-worker stdio transport. Identity, configuration validation,
//! host API negotiation and all network behavior belong to the consuming worker.
//! Frames and queues are bounded; protocol/configuration values have no Debug.

use serde::{de::DeserializeOwned, Deserialize};
use serde_json::Value;
use std::{
    io::{BufRead, Write},
    time::Instant,
};
use tokio::sync::{mpsc, oneshot, watch};

pub const MAX_FRAME_BYTES: usize = 64 * 1024;
pub const QUEUE_CAPACITY: usize = 2;

/// Keep JSON objects distinct from numbers when a consuming worker enables
/// serde_json's arbitrary_precision feature. Its reserved internal map key must
/// never be accepted from an external JSON object. Callers bound bytes first.
pub fn reject_reserved_number_keys(bytes: &[u8]) -> Result<(), &'static str> {
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'"' {
            index += 1;
            continue;
        }
        let start = index;
        index += 1;
        loop {
            match bytes.get(index) {
                Some(b'\\') => index += 2,
                Some(b'"') => {
                    index += 1;
                    break;
                }
                Some(_) => index += 1,
                None => return Err("invalid JSON string"),
            }
        }
        let end = index;
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        if bytes.get(index) == Some(&b':') {
            let key: String =
                serde_json::from_slice(&bytes[start..end]).map_err(|_| "invalid JSON key")?;
            if key == "$serde_json::private::Number" {
                return Err("reserved JSON number key");
            }
        }
    }
    Ok(())
}

// Deliberately no Debug: Configuration contains plaintext credentials.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostFrame<C> {
    Hello {
        protocol_version: u32,
        host_api_version: String,
        plugin_id: String,
    },
    Configuration {
        configuration: C,
    },
    Action {
        request_id: String,
        action_id: String,
        params: Value,
        deadline_ms: u64,
        /// Start the relative deadline when stdin is decoded, before dispatch.
        #[serde(skip, default = "std::time::Instant::now")]
        received_at: std::time::Instant,
    },
    Cancel {
        request_id: String,
    },
    Shutdown {},
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Running,
    Shutdown,
    Eof,
    Invalid,
}

struct Outbound {
    bytes: Vec<u8>,
    written: oneshot::Sender<Result<(), &'static str>>,
    expires_at: Option<Instant>,
}

/// Clones share one bounded queue and its single flush-owning writer thread.
#[derive(Clone)]
pub struct Output {
    sender: mpsc::Sender<Outbound>,
}

impl Output {
    /// Own a supplied writer with the same bounded transport used by stdio.
    /// The writer lives on its own thread; callers never use Tokio's blocking
    /// pool for potentially indefinitely backpressured output.
    pub fn with_writer<W: Write + Send + 'static>(writer: W) -> Self {
        let (sender, outgoing) = mpsc::channel(QUEUE_CAPACITY);
        std::thread::spawn(move || write_frames(writer, outgoing));
        Self { sender }
    }

    pub async fn send(&self, value: Value) -> Result<(), &'static str> {
        let bytes = encode_frame(value)?;
        let (written, result) = oneshot::channel();
        self.sender
            .send(Outbound {
                bytes,
                written,
                expires_at: None,
            })
            .await
            .map_err(|_| "host output closed")?;
        result.await.map_err(|_| "host output closed")?
    }

    /// Best-effort traffic must not block a worker's network event loop.
    /// Handshake/configuration/status use send() and wait for the actual flush.
    pub fn try_send(&self, value: Value) -> Result<bool, &'static str> {
        self.try_enqueue(value, None)
    }

    /// Drop time-sensitive traffic if its original deadline passes while queued.
    /// This bounds the local queue, not an already-started blocking pipe write.
    /// The deadline stays inside this process and never changes the wire format.
    pub fn try_send_before(&self, value: Value, expires_at: Instant) -> Result<bool, &'static str> {
        self.try_enqueue(value, Some(expires_at))
    }

    fn try_enqueue(&self, value: Value, expires_at: Option<Instant>) -> Result<bool, &'static str> {
        let bytes = encode_frame(value)?;
        let (written, _) = oneshot::channel();
        match self.sender.try_send(Outbound {
            bytes,
            written,
            expires_at,
        }) {
            Ok(()) => Ok(true),
            Err(mpsc::error::TrySendError::Full(_)) => Ok(false),
            Err(mpsc::error::TrySendError::Closed(_)) => Err("host output closed"),
        }
    }
}

fn encode_frame(value: Value) -> Result<Vec<u8>, &'static str> {
    let mut bytes = serde_json::to_vec(&value).map_err(|_| "cannot encode worker response")?;
    if bytes.len() >= MAX_FRAME_BYTES {
        return Err("worker response too large");
    }
    bytes.push(b'\n');
    Ok(bytes)
}

/// Own threads, rather than Tokio's blocking stdio pool, allow the process to
/// exit promptly when the host closes stdin while stdout is backpressured.
pub fn stdio<C: DeserializeOwned + Send + 'static>() -> (
    mpsc::Receiver<HostFrame<C>>,
    Output,
    watch::Receiver<StopReason>,
) {
    let (frames, incoming) = mpsc::channel(QUEUE_CAPACITY);
    let (stop, stopped) = watch::channel(StopReason::Running);
    std::thread::spawn(move || {
        let input = std::io::BufReader::with_capacity(4096, std::io::stdin().lock());
        read_frames(input, frames, stop);
    });
    let (sender, outgoing) = mpsc::channel(QUEUE_CAPACITY);
    std::thread::spawn(move || write_frames(std::io::stdout().lock(), outgoing));
    (incoming, Output { sender }, stopped)
}

fn read_frames<R: BufRead, C: DeserializeOwned>(
    mut input: R,
    frames: mpsc::Sender<HostFrame<C>>,
    stop: watch::Sender<StopReason>,
) {
    loop {
        match read_frame(&mut input) {
            Ok(Some(HostFrame::Shutdown {})) => {
                let _ = stop.send(StopReason::Shutdown);
                break;
            }
            Ok(Some(frame)) => {
                if frames.try_send(frame).is_err() {
                    let _ = stop.send(StopReason::Invalid);
                    break;
                }
            }
            Ok(None) => {
                let _ = stop.send(StopReason::Eof);
                break;
            }
            Err(_) => {
                let _ = stop.send(StopReason::Invalid);
                break;
            }
        }
    }
}

fn write_frames<W: Write>(mut output: W, mut outgoing: mpsc::Receiver<Outbound>) {
    while let Some(frame) = outgoing.blocking_recv() {
        if frame
            .expires_at
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            let _ = frame.written.send(Err("worker response expired"));
            continue;
        }
        let result = output
            .write_all(&frame.bytes)
            .and_then(|()| output.flush())
            .map_err(|_| "host output closed");
        let failed = result.is_err();
        let _ = frame.written.send(result);
        if failed {
            break;
        }
    }
}

pub fn read_frame<R: BufRead, C: DeserializeOwned>(
    reader: &mut R,
) -> Result<Option<HostFrame<C>>, &'static str> {
    let mut frame = Vec::new();
    loop {
        let chunk = reader.fill_buf().map_err(|_| "cannot read host frame")?;
        if chunk.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err("unterminated host frame")
            };
        }
        let newline = chunk.iter().position(|byte| *byte == b'\n');
        let length = newline.map_or(chunk.len(), |index| index + 1);
        if frame.len() + length > MAX_FRAME_BYTES {
            return Err("host frame too large");
        }
        frame.extend_from_slice(&chunk[..length]);
        reader.consume(length);
        if newline.is_some() {
            frame.pop();
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
            reject_reserved_number_keys(&frame)?;
            let decoded = serde_json::from_slice(&frame).map_err(|_| "invalid host frame")?;
            validate_command(&decoded)?;
            return Ok(Some(decoded));
        }
    }
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte))
}

fn bounded_params(value: &Value, depth: usize, nodes: &mut usize) -> bool {
    *nodes += 1;
    if depth > 8 || *nodes > 512 {
        return false;
    }
    match value {
        Value::Array(values) => values
            .iter()
            .all(|value| bounded_params(value, depth + 1, nodes)),
        Value::Object(values) => values
            .values()
            .all(|value| bounded_params(value, depth + 1, nodes)),
        _ => true,
    }
}

fn validate_command<C>(frame: &HostFrame<C>) -> Result<(), &'static str> {
    let valid = match frame {
        HostFrame::Action {
            request_id,
            action_id,
            params,
            deadline_ms,
            ..
        } => {
            valid_token(request_id)
                && valid_token(action_id)
                && (1..=30_000).contains(deadline_ms)
                && params.is_object()
                && serde_json::to_vec(params).is_ok_and(|bytes| bytes.len() <= 4096)
                && bounded_params(params, 0, &mut 0)
        }
        HostFrame::Cancel { request_id } => valid_token(request_id),
        _ => true,
    };
    if valid {
        Ok(())
    } else {
        Err("invalid host frame")
    }
}

#[cfg(test)]
mod tests;
