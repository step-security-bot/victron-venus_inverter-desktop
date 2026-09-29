use super::*;
use crate::plugins::http_video::{MAX_CLIP_BYTES, MAX_IMAGE_BYTES};
use crate::plugins::protocol::{HttpVideoGrant, PluginManifest, WorkerConfiguration};
use serde_json::json;
use std::collections::BTreeMap;
use std::io::Write;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::{sleep, timeout};

fn policy() -> TransferPolicy {
    TransferPolicy {
        attempts: 3,
        delays: vec![Duration::from_millis(5)],
        connect_timeout: Duration::from_millis(200),
        read_timeout: Duration::from_millis(100),
        total_timeout: Duration::from_secs(3),
        max_bytes: MAX_CLIP_BYTES,
    }
}

fn lease() -> GenerationLease {
    GenerationLease::new("inverter-desktop.frigate".into(), 1, 1)
}

fn request(base: &str, lease: GenerationLease) -> QueuedHttpVideo {
    let manifest: PluginManifest = serde_json::from_str(include_str!(
        "../../../scripts/plugins/frigate-manifest.json"
    ))
    .unwrap();
    let configuration = WorkerConfiguration {
        revision: "test".into(),
        values: json!({"frigate_base_url": base}),
        secrets: BTreeMap::new(),
    };
    let grant = HttpVideoGrant::from_manifest_configuration(&manifest, Some(&configuration))
        .unwrap()
        .unwrap();
    QueuedHttpVideo {
        created: tokio::time::Instant::now(),
        camera_id: None,
        live_preview: false,
        lease,
        grant,
        id: Uuid::new_v4().to_string(),
        url: format!("{base}/api/events/test/clip.mp4"),
        title: "Front camera".into(),
        media_kind: HttpMediaKind::Video,
    }
}

async fn service(
    policy: TransferPolicy,
) -> (tempfile::TempDir, MediaService, mpsc::Receiver<MediaEvent>) {
    let root = tempfile::tempdir().unwrap();
    let (service, events) = MediaService::new();
    service
        .initialize_with_policy(root.path().canonicalize().unwrap().join("media"), policy)
        .await
        .unwrap();
    (root, service, events)
}

async fn ready(events: &mut mpsc::Receiver<MediaEvent>) -> ReadyMedia {
    match timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap()
    {
        MediaEvent::Ready(ready) => ready,
        MediaEvent::Close { .. } => panic!("expected completed clip before close"),
    }
}

async fn idle(service: &MediaService) {
    timeout(Duration::from_secs(3), async {
        while service.has_owned_work() {
            sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
}

fn response(status: &str, body: &[u8]) -> Vec<u8> {
    let mut bytes = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

async fn broker(responses: Vec<Vec<u8>>) -> (String, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in responses {
            let (mut stream, _) = timeout(Duration::from_secs(3), listener.accept())
                .await
                .unwrap()
                .unwrap();
            // Read available header bytes together so serving a local response
            // does not require one socket operation per byte on Windows.
            let mut request = [0; 8192];
            let mut length = 0;
            while !request[..length].ends_with(b"\r\n\r\n") {
                let received = stream.read(&mut request[length..]).await.unwrap();
                assert_ne!(received, 0, "request closed before its header delimiter");
                length += received;
                assert!(
                    length < request.len(),
                    "request headers exceed fixture bound"
                );
            }
            requests.push(String::from_utf8(request[..length].to_vec()).unwrap());
            stream.write_all(&response).await.unwrap();
            stream.shutdown().await.unwrap();
        }
        requests
    });
    (format!("http://{address}"), task)
}

fn still_image(kind: HttpMediaKind) -> Vec<u8> {
    use base64::Engine;
    // Locally generated one-pixel raster images; no camera or remote fixture.
    let encoded = match kind {
        HttpMediaKind::Jpeg => "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAgGBgcGBQgHBwcJCQgKDBQNDAsLDBkSEw8UHRofHh0aHBwgJC4nICIsIxwcKDcpLDAxNDQ0Hyc5PTgyPC4zNDL/2wBDAQkJCQwLDBgNDRgyIRwhMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjL/wAARCAABAAEDASIAAhEBAxEB/8QAFQABAQAAAAAAAAAAAAAAAAAAAAj/xAAUEAEAAAAAAAAAAAAAAAAAAAAA/8QAFAEBAAAAAAAAAAAAAAAAAAAAAP/EABQRAQAAAAAAAAAAAAAAAAAAAAD/2gAMAwEAAhEDEQA/AJ/AB//Z",
        HttpMediaKind::Png => "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR42mNgYGAAAAAEAAHI6uv5AAAAAElFTkSuQmCC",
        HttpMediaKind::Webp => "UklGRiQAAABXRUJQVlA4IBgAAAAwAQCdASoBAAEAAUAmJaQAA3AA/v02aAA=",
        HttpMediaKind::Video => panic!("expected a raster image fixture"),
    };
    base64::prelude::BASE64_STANDARD.decode(encoded).unwrap()
}

#[tokio::test]
async fn typed_raster_images_have_exact_mime_and_generation_owned_files() {
    for (kind, mime, extension) in [
        (HttpMediaKind::Jpeg, "image/jpeg", "jpg"),
        (HttpMediaKind::Png, "image/png", "png"),
        (HttpMediaKind::Webp, "image/webp", "webp"),
    ] {
        let body = still_image(kind);
        // An upstream generic MIME or misleading URL never chooses the served type.
        let mut reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        ).into_bytes();
        reply.extend_from_slice(&body);
        let (base, server) = broker(vec![reply]).await;
        let (directory, service, mut events) = service(policy()).await;
        let generation = lease();
        let mut image_request = request(&base, generation.clone());
        image_request.media_kind = kind;
        service.try_submit(image_request).unwrap();
        let image = ready(&mut events).await;
        assert_eq!(image.error, None);
        assert_eq!(image.media_kind, kind);
        assert!(image.live_url.is_none());
        let path = directory
            .path()
            .join("media")
            .join(format!("clip-{}.{}", image.media_id, extension));
        assert_eq!(fs::read(&path).unwrap(), body);
        assert!(matches!(
            service
                .read_range(&image.media_id, "main", None, false)
                .await,
            Err(MediaError::NotFound)
        ));
        for head in [false, true] {
            let result = service
                .read_range(&image.media_id, &image.window_label, None, head)
                .await
                .unwrap();
            assert_eq!(result.status, 200);
            assert_eq!(result.content_type, mime);
            assert_eq!(result.content_length, body.len() as u64);
            assert_eq!(
                result.bytes.as_slice(),
                if head { &[] } else { body.as_slice() }
            );
        }
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].to_lowercase().contains("authorization:"));
        generation.revoke();
        assert!(matches!(
            service
                .read_range(&image.media_id, &image.window_label, None, false)
                .await,
            Err(MediaError::NotFound)
        ));
        match timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap()
        {
            MediaEvent::Close { window_label } => {
                assert_eq!(window_label, image.window_label);
                // Retirement keeps the owned file until native absence is acknowledged.
                assert!(path.exists());
                service.window_destroyed(&window_label);
            }
            MediaEvent::Ready(_) => panic!("expected generation-owned close"),
        }
        idle(&service).await;
        assert!(!path.exists());
        service.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn image_kind_mismatches_markup_and_short_signatures_are_terminal() {
    for (kind, body) in [
        (HttpMediaKind::Jpeg, still_image(HttpMediaKind::Png)),
        (HttpMediaKind::Png, still_image(HttpMediaKind::Webp)),
        (HttpMediaKind::Webp, still_image(HttpMediaKind::Jpeg)),
        (
            HttpMediaKind::Jpeg,
            b"<svg xmlns='http://www.w3.org/2000/svg'/>".to_vec(),
        ),
        (
            HttpMediaKind::Png,
            b"<!doctype html><script>alert(1)</script>".to_vec(),
        ),
        (HttpMediaKind::Webp, b"RIFF\0\0\0\0WAVE".to_vec()),
        (HttpMediaKind::Jpeg, vec![0xff, 0xd8]),
        (HttpMediaKind::Png, b"\x89PNG\r\n\x1a".to_vec()),
        (HttpMediaKind::Webp, b"RIFF\0\0\0\0WEB".to_vec()),
    ] {
        let (base, server) = broker(vec![response("200 OK", &body)]).await;
        let (directory, service, mut events) = service(policy()).await;
        let mut image_request = request(&base, lease());
        image_request.media_kind = kind;
        service.try_submit(image_request).unwrap();
        let image = ready(&mut events).await;
        assert_eq!(
            image.error,
            Some(MediaError::Transfer(VideoError::InvalidMedia))
        );
        assert_eq!(server.await.unwrap().len(), 1);
        assert_eq!(
            fs::read_dir(directory.path().join("media"))
                .unwrap()
                .count(),
            0
        );
        assert!(matches!(
            service
                .read_range(&image.media_id, &image.window_label, None, false)
                .await,
            Err(MediaError::NotFound)
        ));
        service.window_failed(&image.media_id);
        idle(&service).await;
        service.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn image_full_responses_above_video_range_limit_keep_read_permits_bounded() {
    let mut body = still_image(HttpMediaKind::Png);
    body.resize(MAX_RANGE_BYTES + 100, 0);
    let (base, server) = broker(vec![response("200 OK", &body)]).await;
    let (_directory, service, mut events) = service(policy()).await;
    let mut image_request = request(&base, lease());
    image_request.media_kind = HttpMediaKind::Png;
    service.try_submit(image_request).unwrap();
    let image = ready(&mut events).await;
    assert_eq!(image.error, None);
    let mut held = Vec::new();
    for _ in 0..MAX_RANGE_READS {
        let result = service
            .read_range(&image.media_id, &image.window_label, None, false)
            .await
            .unwrap();
        assert_eq!(result.status, 200);
        assert_eq!(result.content_type, "image/png");
        assert_eq!(result.bytes, body);
        held.push(result);
    }
    assert!(matches!(
        service
            .read_range(&image.media_id, &image.window_label, None, false)
            .await,
        Err(MediaError::Busy)
    ));
    drop(held);
    let partial = service
        .read_range(
            &image.media_id,
            &image.window_label,
            Some("bytes=0-"),
            false,
        )
        .await
        .unwrap();
    assert_eq!(partial.status, 206);
    assert_eq!(partial.bytes.len(), MAX_RANGE_BYTES);
    assert_eq!(partial.content_type, "image/png");
    drop(partial);
    server.await.unwrap();
    service.window_destroyed(&image.window_label);
    idle(&service).await;
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn image_cap_applies_to_declared_and_streamed_bodies_independently_of_video_limit() {
    let mut body = still_image(HttpMediaKind::Png);
    body.resize(MAX_IMAGE_BYTES as usize + 1, 0);
    let mut streamed = format!(
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n",
        body.len()
    )
    .into_bytes();
    streamed.extend_from_slice(&body);
    streamed.extend_from_slice(b"\r\n0\r\n\r\n");
    for reply in [
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            MAX_IMAGE_BYTES + 1
        )
        .into_bytes(),
        streamed,
    ] {
        let (base, server) = broker(vec![reply]).await;
        let mut limits = policy();
        limits.read_timeout = Duration::from_secs(5);
        limits.total_timeout = Duration::from_secs(10);
        let (directory, service, mut events) = service(limits).await;
        let mut image_request = request(&base, lease());
        image_request.media_kind = HttpMediaKind::Png;
        service.try_submit(image_request).unwrap();
        let image = match timeout(Duration::from_secs(15), events.recv())
            .await
            .unwrap()
            .unwrap()
        {
            MediaEvent::Ready(image) => image,
            MediaEvent::Close { .. } => panic!("expected bounded image transfer result"),
        };
        assert_eq!(
            image.error,
            Some(MediaError::Transfer(VideoError::Oversized))
        );
        assert_eq!(server.await.unwrap().len(), 1);
        assert_eq!(
            fs::read_dir(directory.path().join("media"))
                .unwrap()
                .count(),
            0
        );
        service.window_failed(&image.media_id);
        idle(&service).await;
        service.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn retries_pending_recording_empty_and_truncated_body_then_serves_exact_bytes() {
    let (base, server) = broker(vec![
        response("400 Bad Request", b"recording pending"),
        b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\nConnection: close\r\n\r\ntruncated".to_vec(),
        response("200 OK", b"complete-video"),
    ])
    .await;
    let (directory, service, mut events) = service(policy()).await;
    service.try_submit(request(&base, lease())).unwrap();
    let clip = ready(&mut events).await;
    assert_eq!(clip.error, None);
    let bytes = service
        .read_range(&clip.media_id, &clip.window_label, None, false)
        .await
        .unwrap();
    assert_eq!(bytes.bytes, b"complete-video");
    assert_eq!(bytes.status, 200);
    drop(bytes);
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests
        .iter()
        .all(|request| !request.to_lowercase().contains("authorization:")));
    service.window_destroyed(&clip.window_label);
    idle(&service).await;
    service.shutdown().await.unwrap();
    assert_eq!(
        fs::read_dir(directory.path().join("media"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn empty_body_is_retried_and_file_is_replaced_before_success() {
    let (base, server) = broker(vec![response("200 OK", b""), response("200 OK", b"video")]).await;
    let (directory, service, mut events) = service(policy()).await;
    service.try_submit(request(&base, lease())).unwrap();
    let clip = ready(&mut events).await;
    assert_eq!(clip.error, None);
    let bytes = service
        .read_range(&clip.media_id, &clip.window_label, None, false)
        .await
        .unwrap();
    assert_eq!(bytes.bytes, b"video");
    assert_eq!(bytes.status, 200);
    drop(bytes);
    assert_eq!(server.await.unwrap().len(), 2);
    service.window_failed(&clip.media_id);
    idle(&service).await;
    service.shutdown().await.unwrap();
    assert_eq!(
        fs::read_dir(directory.path().join("media"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn oversized_declared_or_progressive_bodies_do_not_retry_or_leave_files() {
    for response in [
        b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n".to_vec(),
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n8\r\n12345678\r\n0\r\n\r\n".to_vec(),
    ] {
        let (base, server) = broker(vec![response]).await;
        let mut limits = policy();
        limits.max_bytes = 7;
        let (directory, service, mut events) = service(limits).await;
        service.try_submit(request(&base, lease())).unwrap();
        let clip = ready(&mut events).await;
        assert_eq!(clip.error, Some(MediaError::Transfer(VideoError::Oversized)));
        assert_eq!(server.await.unwrap().len(), 1);
        assert_eq!(fs::read_dir(directory.path().join("media")).unwrap().count(), 0);
        service.window_failed(&clip.media_id);
        idle(&service).await;
        service.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn revocation_cancels_stalled_body_without_opening_a_window() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (started, observed) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = [0; 8192];
        assert!(stream.read(&mut bytes).await.unwrap() > 0);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nx")
            .await
            .unwrap();
        started.send(()).unwrap();
        let _ = stream.read(&mut bytes).await;
    });
    let (directory, service, mut events) = service(policy()).await;
    let generation = lease();
    service
        .try_submit(request(&base, generation.clone()))
        .unwrap();
    observed.await.unwrap();
    generation.revoke();
    idle(&service).await;
    assert!(events.try_recv().is_err());
    service.shutdown().await.unwrap();
    assert_eq!(
        fs::read_dir(directory.path().join("media"))
            .unwrap()
            .count(),
        0
    );
    timeout(Duration::from_secs(1), server)
        .await
        .unwrap()
        .unwrap();
}

async fn frozen_io<T>(phase: &str, operation: impl std::future::Future<Output = T>) -> T {
    let started = std::time::Instant::now();
    let virtual_started = tokio::time::Instant::now();
    tokio::pin!(operation);
    loop {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "fixture I/O stalled: {phase}"
        );
        tokio::select! {
            biased;
            result = &mut operation => {
                assert_eq!(tokio::time::Instant::now(), virtual_started);
                return result;
            }
            // Stay runnable so external socket and disk waits cannot advance test time.
            _ = tokio::task::yield_now() => {}
        }
    }
}

async fn frozen_request(stream: &mut tokio::net::TcpStream) -> String {
    frozen_io("request headers", async {
        let mut bytes = [0; 8192];
        let mut length = 0;
        while !bytes[..length].ends_with(b"\r\n\r\n") {
            assert!(length < bytes.len(), "request headers exceed fixture bound");
            let received = stream.read(&mut bytes[length..]).await.unwrap();
            assert_ne!(received, 0, "request closed before its header delimiter");
            length += received;
        }
        String::from_utf8(bytes[..length].to_vec()).unwrap()
    })
    .await
}

async fn frozen_prefix(path: &std::path::Path, expected: &[u8]) {
    frozen_io("persisted response prefix", async {
        loop {
            let path = path.to_owned();
            let bytes = tokio::task::spawn_blocking(move || fs::read(path))
                .await
                .unwrap()
                .unwrap();
            assert!(
                expected.starts_with(&bytes),
                "unexpected partial response bytes"
            );
            if bytes == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn progressive_progress_survives_idle_limit_but_stall_retries() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let limits = policy();
    let (directory, service, mut events) =
        frozen_io("initialize media service", service(limits.clone())).await;
    service.try_submit(request(&base, lease())).unwrap();

    let (mut stalled, _) = frozen_io("first connection", listener.accept())
        .await
        .unwrap();
    let media_directory = directory.path().join("media");
    let mut files = frozen_io(
        "locate partial clip",
        tokio::task::spawn_blocking(move || {
            fs::read_dir(media_directory)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>()
        }),
    )
    .await
    .unwrap();
    assert_eq!(files.len(), 1);
    let path = files.pop().unwrap();
    let mut requests = vec![frozen_request(&mut stalled).await];
    frozen_io(
        "incomplete response",
        stalled.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nx"),
    )
    .await
    .unwrap();
    frozen_prefix(&path, b"x").await;
    // Let ready tasks and the blocking disk write settle before advancing to the
    // idle timer. Immediate advance could run before the next read timer is armed.
    sleep(limits.read_timeout + Duration::from_millis(1)).await;
    let mut closed = [0; 1];
    assert_eq!(
        frozen_io("idle timeout closes client", stalled.read(&mut closed))
            .await
            .unwrap(),
        0
    );
    drop(stalled);
    sleep(limits.delays[0]).await;

    let (mut progressive, _) = frozen_io("retry connection", listener.accept())
        .await
        .unwrap();
    requests.push(frozen_request(&mut progressive).await);
    frozen_io(
        "retry headers",
        progressive.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n"),
    )
    .await
    .unwrap();
    let progress_started = tokio::time::Instant::now();
    for (index, byte) in b"video".iter().enumerate() {
        if index != 0 {
            tokio::time::advance(Duration::from_millis(30)).await;
        }
        frozen_io("progressive body byte", progressive.write_all(&[*byte]))
            .await
            .unwrap();
        frozen_prefix(&path, &b"video"[..=index]).await;
    }
    assert_eq!(progress_started.elapsed(), Duration::from_millis(120));
    assert!(progress_started.elapsed() > limits.read_timeout);
    frozen_io("finish response", progressive.shutdown())
        .await
        .unwrap();
    drop(progressive);
    let clip = frozen_io("ready clip", ready(&mut events)).await;
    assert_eq!(clip.error, None);
    let bytes = frozen_io(
        "read completed clip",
        service.read_range(&clip.media_id, &clip.window_label, None, false),
    )
    .await
    .unwrap();
    assert_eq!(bytes.bytes, b"video");
    assert_eq!(bytes.status, 200);
    drop(bytes);
    assert_eq!(requests.len(), 2);
    assert!(requests
        .iter()
        .all(|request| request.starts_with("GET /api/events/test/clip.mp4 HTTP/1.1\r\n")));
    assert_eq!(
        listener.into_std().unwrap().accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    service.window_failed(&clip.media_id);
    frozen_io("media cleanup", async {
        while service.has_owned_work() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    frozen_io("service shutdown", service.shutdown())
        .await
        .unwrap();
    assert_eq!(
        fs::read_dir(directory.path().join("media"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn overall_deadline_includes_backoff_and_redirects_are_not_followed() {
    for (reply, expected) in [
        (response("400 Bad Request", b"wait"), VideoError::Deadline),
        (b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(), VideoError::Http),
    ] {
        let (base, server) = broker(vec![reply]).await;
        let mut limits = policy();
        limits.total_timeout = Duration::from_millis(100);
        limits.delays = vec![Duration::from_secs(1)];
        let (_directory, service, mut events) = service(limits).await;
        service.try_submit(request(&base, lease())).unwrap();
        let clip = ready(&mut events).await;
        assert_eq!(clip.error, Some(MediaError::Transfer(expected)));
        assert_eq!(server.await.unwrap().len(), 1);
        service.window_failed(&clip.media_id);
        idle(&service).await;
        service.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn ranges_are_window_bound_bounded_and_revoked_immediately() {
    let body = vec![42; MAX_RANGE_BYTES + 100];
    let (base, server) = broker(vec![response("200 OK", &body)]).await;
    let (_directory, service, mut events) = service(policy()).await;
    let generation = lease();
    service
        .try_submit(request(&base, generation.clone()))
        .unwrap();
    let clip = ready(&mut events).await;
    assert!(service.window_ready(&clip.media_id, &clip.window_label));
    assert!(matches!(
        service
            .read_range(&clip.media_id, "main", Some("bytes=0-9"), false)
            .await,
        Err(MediaError::NotFound)
    ));
    assert!(matches!(
        service
            .read_range(&clip.media_id, &clip.window_label, None, false)
            .await,
        Err(MediaError::RangeRequired)
    ));
    let head = service
        .read_range(&clip.media_id, &clip.window_label, None, true)
        .await
        .unwrap();
    assert_eq!(head.content_length, body.len() as u64);
    assert!(head.bytes.is_empty());
    let mut held = Vec::new();
    for _ in 0..MAX_RANGE_READS {
        let response = service
            .read_range(&clip.media_id, &clip.window_label, Some("bytes=0-"), false)
            .await
            .unwrap();
        assert_eq!(response.bytes.len(), MAX_RANGE_BYTES);
        assert_eq!(response.status, 206);
        held.push(response);
    }
    assert!(matches!(
        service
            .read_range(&clip.media_id, &clip.window_label, Some("bytes=0-1"), false)
            .await,
        Err(MediaError::Busy)
    ));
    drop(held);
    let suffix = service
        .read_range(&clip.media_id, &clip.window_label, Some("bytes=-9"), false)
        .await
        .unwrap();
    assert_eq!(suffix.bytes, [42; 9]);
    drop(suffix);
    let invalid = service
        .read_range(
            &clip.media_id,
            &clip.window_label,
            Some("bytes=99999999-"),
            false,
        )
        .await
        .unwrap();
    assert_eq!(invalid.status, 416);
    assert_eq!(
        invalid.content_range,
        Some(format!("bytes */{}", body.len()))
    );
    generation.revoke();
    assert!(!service.is_window_active(&clip.media_id, &clip.window_label));
    assert!(!service.window_ready(&clip.media_id, &clip.window_label));
    assert!(!service.window_closing(&clip.media_id, &clip.window_label));
    assert!(matches!(
        service
            .read_range(&clip.media_id, &clip.window_label, Some("bytes=0-1"), false)
            .await,
        Err(MediaError::NotFound)
    ));
    match timeout(Duration::from_secs(1), events.recv())
        .await
        .unwrap()
        .unwrap()
    {
        MediaEvent::Close { window_label } => {
            assert_eq!(window_label, clip.window_label);
            service.window_destroyed(&window_label);
        }
        _ => panic!("expected generation-owned close"),
    }
    server.await.unwrap();
    idle(&service).await;
    service.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn failed_window_preserves_sibling_and_shutdown_waits_for_close_acknowledgement() {
    let (base, server) = broker(vec![response("200 OK", b"one"), response("200 OK", b"two")]).await;
    // This exercises window ownership; real socket/disk scheduling must not
    // consume the short transfer deadlines tested separately above.
    let (directory, service, mut events) =
        frozen_io("initialize sibling media service", service(policy())).await;
    service.try_submit(request(&base, lease())).unwrap();
    let first = frozen_io("first sibling ready", ready(&mut events)).await;
    assert_eq!(first.error, None);
    service.try_submit(request(&base, lease())).unwrap();
    let second = frozen_io("second sibling ready", ready(&mut events)).await;
    assert_eq!(second.error, None);
    service.window_failed(&first.media_id);
    assert_eq!(
        frozen_io(
            "read surviving sibling",
            service.read_range(&second.media_id, &second.window_label, None, false),
        )
        .await
        .unwrap()
        .bytes,
        b"two"
    );
    let closing = service.clone();
    let task = tokio::spawn(async move { closing.shutdown().await });
    let label = match frozen_io(
        "shutdown sibling close request",
        timeout(Duration::from_secs(1), events.recv()),
    )
    .await
    .unwrap()
    .unwrap()
    {
        MediaEvent::Close { window_label } => window_label,
        _ => panic!("shutdown must close owned sibling"),
    };
    assert_eq!(label, second.window_label);
    assert!(!task.is_finished());
    service.window_destroyed(&label);
    frozen_io("acknowledged sibling shutdown", task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        frozen_io("sibling requests", server).await.unwrap().len(),
        2
    );
    assert_eq!(
        fs::read_dir(directory.path().join("media"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn native_close_failure_returns_error_retains_ownership_and_can_retry() {
    let (base, server) = broker(vec![response("200 OK", b"video")]).await;
    let (directory, service, mut events) = service(policy()).await;
    service.try_submit(request(&base, lease())).unwrap();
    let clip = ready(&mut events).await;
    let closing = service.clone();
    let first = tokio::spawn(async move { closing.shutdown().await });
    match timeout(Duration::from_secs(1), events.recv())
        .await
        .unwrap()
        .unwrap()
    {
        MediaEvent::Close { window_label } => service.window_cleanup_failed(&window_label),
        _ => panic!("expected native close attempt"),
    }
    assert!(timeout(Duration::from_secs(1), first)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    assert!(service.has_owned_work());
    assert_eq!(service.0.tasks.lock().unwrap().len(), 1);
    assert_eq!(
        fs::read_dir(directory.path().join("media"))
            .unwrap()
            .count(),
        1
    );
    assert!(!service.is_window_active(&clip.media_id, &clip.window_label));
    let closing = service.clone();
    let second = tokio::spawn(async move { closing.shutdown().await });
    match timeout(Duration::from_secs(1), events.recv())
        .await
        .unwrap()
        .unwrap()
    {
        MediaEvent::Close { window_label } => {
            assert_eq!(window_label, clip.window_label);
            service.window_destroyed(&window_label);
        }
        _ => panic!("retry must close the same still-owned window"),
    }
    timeout(Duration::from_secs(1), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!service.has_owned_work());
    assert_eq!(
        fs::read_dir(directory.path().join("media"))
            .unwrap()
            .count(),
        0
    );
    server.await.unwrap();
}

#[tokio::test]
async fn cancelled_shutdown_keeps_unjoined_tasks_for_later_drain() {
    let (base, server) = broker(vec![response("200 OK", b"video")]).await;
    let (_directory, service, mut events) = service(policy()).await;
    service.try_submit(request(&base, lease())).unwrap();
    let clip = ready(&mut events).await;
    let closing = service.clone();
    let attempt = tokio::spawn(async move { closing.shutdown().await });
    assert!(matches!(
        timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap(),
        Some(MediaEvent::Close { .. })
    ));
    attempt.abort();
    let _ = attempt.await;
    assert_eq!(service.0.tasks.lock().unwrap().len(), 1);
    service.window_destroyed(&clip.window_label);
    service.shutdown().await.unwrap();
    assert!(!service.has_owned_work());
    server.await.unwrap();
}

#[tokio::test]
async fn full_storage_keeps_eight_requests_queued_without_network_or_disk_work() {
    let (directory, service, _events) = service(policy()).await;
    {
        let mut state = service.0.state.lock().unwrap();
        for _ in 0..2 {
            state.items.insert(
                Uuid::new_v4().to_string(),
                Item {
                    camera_id: None,
                    lease: lease(),
                    label: "occupied".into(),
                    phase: Phase::Ready,
                    media_kind: HttpMediaKind::Video,
                    bytes: MAX_CLIP_BYTES,
                    file: None,
                    live_url: None,
                    retired: watch::channel(false).0,
                    cleanup_failed: false,
                },
            );
        }
    }
    let generation = lease();
    for _ in 0..MAX_QUEUED {
        service
            .try_submit(request("http://127.0.0.1:1", generation.clone()))
            .unwrap();
    }
    assert_eq!(
        service.try_submit(request("http://127.0.0.1:1", generation.clone())),
        Err(MediaError::Busy)
    );
    tokio::task::yield_now().await;
    assert_eq!(
        fs::read_dir(directory.path().join("media"))
            .unwrap()
            .count(),
        0
    );
    generation.revoke();
    service
        .0
        .state
        .lock()
        .unwrap()
        .items
        .retain(|_, item| item.label != "occupied");
    idle(&service).await;
    service.shutdown().await.unwrap();
}

#[test]
fn ranges_reject_overflow_multiple_ranges_and_non_decimal_tokens() {
    for range in [
        "bytes=0-1,4-5",
        "bytes=-0",
        "bytes=2-1",
        "bytes=+1-2",
        "bytes=0-18446744073709551616",
        "items=0-2",
    ] {
        assert_eq!(
            select_range(Some(range), 10, MAX_RANGE_BYTES as u64).unwrap(),
            None,
            "{range}"
        );
    }
    assert_eq!(
        select_range(Some("bytes=-30"), 10, MAX_RANGE_BYTES as u64).unwrap(),
        Some((0, 9, true))
    );
    assert_eq!(
        select_range(Some("bytes=4-100"), 10, MAX_RANGE_BYTES as u64).unwrap(),
        Some((4, 9, true))
    );
}

#[tokio::test]
async fn eight_windows_backpressure_later_clips_until_native_close_is_acknowledged() {
    let (base, server) = broker((0..9).map(|_| response("200 OK", b"video")).collect()).await;
    let (_directory, service, mut events) = service(policy()).await;
    let mut clips = Vec::new();
    for _ in 0..MAX_WINDOWS {
        service.try_submit(request(&base, lease())).unwrap();
        clips.push(ready(&mut events).await);
    }
    service.try_submit(request(&base, lease())).unwrap();
    {
        let state = service.0.state.lock().unwrap();
        assert_eq!(
            state
                .items
                .values()
                .filter(|item| item.phase == Phase::Ready)
                .count(),
            MAX_WINDOWS
        );
        assert_eq!(
            state
                .items
                .values()
                .filter(|item| item.phase == Phase::Queued)
                .count(),
            1
        );
    }
    service.window_destroyed(&clips.remove(0).window_label);
    clips.push(ready(&mut events).await);
    for clip in clips {
        service.window_failed(&clip.media_id);
    }
    idle(&service).await;
    assert_eq!(server.await.unwrap().len(), 9);
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn two_transfers_hold_budget_and_revocation_cancels_third_queued_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (accepted, mut observing) = mpsc::channel(2);
    let server = tokio::spawn(async move {
        let mut streams = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 8192];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nx")
                .await
                .unwrap();
            streams.push(stream);
            accepted.send(()).await.unwrap();
        }
        std::future::pending::<()>().await;
        drop(streams);
    });
    let mut limits = policy();
    limits.read_timeout = Duration::from_secs(3);
    let (_directory, service, mut events) = service(limits).await;
    let generation = lease();
    for _ in 0..2 {
        service
            .try_submit(request(&base, generation.clone()))
            .unwrap();
    }
    observing.recv().await.unwrap();
    observing.recv().await.unwrap();
    service
        .try_submit(request(&base, generation.clone()))
        .unwrap();
    {
        let state = service.0.state.lock().unwrap();
        assert_eq!(
            state
                .items
                .values()
                .filter(|item| item.phase == Phase::Downloading)
                .count(),
            2
        );
        assert_eq!(
            state.items.values().map(|item| item.bytes).sum::<u64>(),
            MAX_MEDIA_BYTES
        );
        assert_eq!(
            state
                .items
                .values()
                .filter(|item| item.phase == Phase::Queued)
                .count(),
            1
        );
    }
    generation.revoke();
    idle(&service).await;
    assert!(events.try_recv().is_err());
    service.shutdown().await.unwrap();
    server.abort();
    let _ = server.await;
}

fn orphan(directory: &Path) -> PathBuf {
    orphan_with_extension(directory, "mp4")
}

fn orphan_with_extension(directory: &Path, extension: &str) -> PathBuf {
    let path = directory.join(format!("clip-{}.{extension}", Uuid::new_v4()));
    let mut file = private_options()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    file.write_all(b"partial orphan").unwrap();
    path
}

#[tokio::test]
async fn initialization_recovers_only_safe_owned_orphans_and_preserves_unknown_files() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().canonicalize().unwrap().join("media");
    recover_directory(&directory).unwrap();
    let old: Vec<_> = ["mp4", "jpg", "png", "webp"]
        .into_iter()
        .map(|extension| orphan_with_extension(&directory, extension))
        .collect();
    let (service, _events) = MediaService::new();
    service.initialize(directory.clone()).await.unwrap();
    assert!(old.iter().all(|path| !path.exists()));
    service.shutdown().await.unwrap();
    let safe = orphan(&directory);
    fs::write(directory.join("unowned.txt"), b"preserve").unwrap();
    let (blocked, _events) = MediaService::new();
    assert!(blocked.initialize(directory.clone()).await.is_err());
    assert!(safe.exists());
    assert!(directory.join("unowned.txt").exists());
    assert!(!blocked.has_owned_work());
    blocked.shutdown().await.unwrap();
}

#[tokio::test]
async fn recovery_preserves_unsupported_image_extensions() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().canonicalize().unwrap().join("media");
    recover_directory(&directory).unwrap();
    let safe = orphan_with_extension(&directory, "png");
    let unknown = orphan_with_extension(&directory, "svg");
    let (service, _events) = MediaService::new();
    assert!(service.initialize(directory).await.is_err());
    assert!(safe.exists());
    assert!(unknown.exists());
    service.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn recovery_rejects_symlinks_hardlinks_and_nonprivate_files_without_deletion() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    for kind in ["symlink", "hardlink", "mode"] {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().canonicalize().unwrap().join("media");
        recover_directory(&directory).unwrap();
        let outside = root.path().join("outside");
        fs::write(&outside, b"preserve").unwrap();
        let path = directory.join(format!("clip-{}.mp4", Uuid::new_v4()));
        match kind {
            "symlink" => symlink(&outside, &path).unwrap(),
            "hardlink" => fs::hard_link(&outside, &path).unwrap(),
            _ => {
                fs::write(&path, b"preserve").unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            }
        }
        let (service, _events) = MediaService::new();
        assert!(service.initialize(directory).await.is_err());
        assert!(path.symlink_metadata().is_ok());
        assert_eq!(fs::read(&outside).unwrap(), b"preserve");
        service.shutdown().await.unwrap();
    }
}

fn live_request_with_duration(base: &str, lease: GenerationLease, seconds: u16) -> QueuedHttpVideo {
    let mut manifest: PluginManifest = serde_json::from_str(include_str!(
        "../../../scripts/plugins/frigate-manifest.json"
    ))
    .unwrap();
    manifest.http_video.as_mut().unwrap().live_preview =
        Some(crate::plugins::protocol::LivePreviewDeclaration {
            query: "fps=2&height=360".into(),
            max_duration_seconds: seconds,
        });
    let configuration = WorkerConfiguration {
        revision: "preview-fixture".into(),
        values: json!({"frigate_base_url":base}),
        secrets: BTreeMap::new(),
    };
    let mut request = live_request(base, lease);
    request.grant = HttpVideoGrant::from_manifest_configuration(&manifest, Some(&configuration))
        .unwrap()
        .unwrap();
    request
}

#[tokio::test]
async fn preview_lifetime_uses_its_verified_grant_and_holds_closing_slot() {
    let (_directory, service, mut events) = service(policy()).await;
    service
        .try_submit(live_request_with_duration("http://127.0.0.1:1", lease(), 5))
        .unwrap();
    let preview = ready(&mut events).await;
    tokio::time::pause();
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(4)).await;
    assert!(events.try_recv().is_err());
    assert!(service.is_window_active(&preview.media_id, &preview.window_label));
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    let MediaEvent::Close { window_label } = events.recv().await.unwrap() else {
        panic!("grant expiry must close preview")
    };
    assert_eq!(window_label, preview.window_label);
    assert!(!service.is_window_active(&preview.media_id, &window_label));
    assert!(service.has_owned_work());
    service.window_destroyed(&window_label);
    tokio::time::resume();
    idle(&service).await;
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn preview_needs_only_window_capacity_while_clip_transfers_and_bytes_are_full() {
    let (_directory, service, mut events) = service(policy()).await;
    {
        let mut state = service.0.state.lock().unwrap();
        for _ in 0..MAX_TRANSFERS {
            state.items.insert(
                Uuid::new_v4().to_string(),
                Item {
                    camera_id: None,
                    lease: lease(),
                    label: "occupied-clip".into(),
                    phase: Phase::Downloading,
                    media_kind: HttpMediaKind::Video,
                    bytes: MAX_CLIP_BYTES,
                    file: None,
                    live_url: None,
                    retired: watch::channel(false).0,
                    cleanup_failed: false,
                },
            );
        }
    }
    service
        .try_submit(live_request("http://127.0.0.1:1", lease()))
        .unwrap();
    let preview = ready(&mut events).await;
    {
        let mut state = service.0.state.lock().unwrap();
        assert_eq!(
            state.items.values().map(|item| item.bytes).sum::<u64>(),
            MAX_MEDIA_BYTES
        );
        assert_eq!(state.items[&preview.media_id].bytes, 0);
        assert!(state.items[&preview.media_id].file.is_none());
        state.items.retain(|_, item| item.label != "occupied-clip");
    }
    service.window_destroyed(&preview.window_label);
    idle(&service).await;
    service.shutdown().await.unwrap();
}

fn live_request(base: &str, lease: GenerationLease) -> QueuedHttpVideo {
    let mut request = request(base, lease);
    request.live_preview = true;
    request.url = format!("{base}/api/front?fps=2&height=360");
    request
}

fn camera_request(owner: GenerationLease, camera: MediaCameraId, event: &str) -> QueuedHttpVideo {
    let mut request = live_request("http://127.0.0.1:1", owner);
    request.camera_id = Some(camera);
    request.id = event.into();
    request
}

#[tokio::test]
async fn same_camera_is_reserved_until_native_destruction_across_generation_changes() {
    for camera in [
        MediaCameraId::Explicit("front".into()),
        MediaCameraId::HttpPreview("http://127.0.0.1:1/api/front".into()),
    ] {
        let (_directory, service, mut events) = service(policy()).await;
        let owner = lease();
        service
            .try_submit(camera_request(owner.clone(), camera.clone(), "first"))
            .unwrap();
        service
            .try_submit(camera_request(
                owner.clone(),
                camera.clone(),
                "queued-repeat",
            ))
            .unwrap();
        assert_eq!(service.0.state.lock().unwrap().items.len(), 1);
        let first = ready(&mut events).await;
        assert!(service.window_ready(&first.media_id, &first.window_label));
        let mut repeat = camera_request(owner.clone(), camera.clone(), "hidden-repeat");
        repeat.title = "A changed camera title".into();
        service.try_submit(repeat).unwrap();
        assert!(events.try_recv().is_err());
        assert_eq!(service.0.state.lock().unwrap().items.len(), 1);

        let sibling_owner = lease();
        service
            .try_submit(camera_request(
                sibling_owner.clone(),
                MediaCameraId::Explicit("other-camera".into()),
                "sibling",
            ))
            .unwrap();
        let sibling = ready(&mut events).await;
        assert_eq!(service.0.state.lock().unwrap().items.len(), 2);

        assert!(service.window_closing(&first.media_id, &first.window_label));
        owner.revoke();
        let MediaEvent::Close { window_label } = events.recv().await.unwrap() else {
            panic!("expected revoked camera close")
        };
        assert_eq!(window_label, first.window_label);
        let replacement_owner = lease();
        service
            .try_submit(camera_request(
                replacement_owner.clone(),
                camera.clone(),
                "closing-repeat",
            ))
            .unwrap();
        assert!(events.try_recv().is_err());
        assert_eq!(
            service.0.state.lock().unwrap().items.len(),
            2,
            "new generations cannot overlap a closing native camera window"
        );
        assert!(service.window_ready(&sibling.media_id, &sibling.window_label));

        service.window_destroyed(&first.window_label);
        timeout(Duration::from_secs(1), async {
            while service
                .0
                .state
                .lock()
                .unwrap()
                .items
                .contains_key(&first.media_id)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        service
            .try_submit(camera_request(replacement_owner, camera, "new-motion"))
            .unwrap();
        let next = ready(&mut events).await;
        assert_ne!(next.window_label, first.window_label);
        assert!(
            events.try_recv().is_err(),
            "suppressed repeats never replay"
        );
        service.window_destroyed(&sibling.window_label);
        service.window_destroyed(&next.window_label);
        idle(&service).await;
        service.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_camera_events_create_one_item_and_provider_namespaces_stay_independent() {
    let (_directory, service, mut events) = service(policy()).await;
    let owner = lease();
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let mut submissions = Vec::new();
    for index in 0..8 {
        let service = service.clone();
        let owner = owner.clone();
        let barrier = barrier.clone();
        submissions.push(tokio::spawn(async move {
            barrier.wait().await;
            service.try_submit(camera_request(
                owner,
                MediaCameraId::Explicit("front".into()),
                &format!("motion-{index}"),
            ))
        }));
    }
    for submission in submissions {
        submission.await.unwrap().unwrap();
    }
    let first = ready(&mut events).await;
    assert_eq!(service.0.state.lock().unwrap().items.len(), 1);
    assert!(events.try_recv().is_err());

    service
        .try_submit(camera_request(
            GenerationLease::new("inverter-desktop.kerberos".into(), 1, 1),
            MediaCameraId::Explicit("front".into()),
            "other-provider",
        ))
        .unwrap();
    let second = ready(&mut events).await;
    assert_eq!(service.0.state.lock().unwrap().items.len(), 2);
    service.window_destroyed(&first.window_label);
    service.window_destroyed(&second.window_label);
    idle(&service).await;
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn live_url_read_requires_the_exact_active_owner_and_revokes_before_native_close() {
    let (_directory, service, mut events) = service(policy()).await;
    let owner = lease();
    service
        .try_submit(live_request("http://127.0.0.1:1", owner.clone()))
        .unwrap();
    let first = ready(&mut events).await;
    service
        .try_submit(live_request("http://127.0.0.1:1", owner.clone()))
        .unwrap();
    let second = ready(&mut events).await;
    assert_eq!(
        service
            .live_preview_url(&first.media_id, &first.window_label)
            .unwrap()
            .as_str(),
        "http://127.0.0.1:1/api/front?fps=2&height=360"
    );
    for wrong in ["main", "config", second.window_label.as_str()] {
        assert!(service.live_preview_url(&first.media_id, wrong).is_none());
    }
    service.window_destroyed(&first.window_label);
    assert!(service
        .live_preview_url(&first.media_id, &first.window_label)
        .is_none());
    owner.revoke();
    assert!(service
        .live_preview_url(&second.media_id, &second.window_label)
        .is_none());
    let MediaEvent::Close { window_label } = events.recv().await.unwrap() else {
        panic!("expected revoked preview close")
    };
    assert_eq!(window_label, second.window_label);
    assert!(service.has_owned_work());
    service.window_destroyed(&window_label);
    idle(&service).await;
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn hidden_window_expiry_revokes_reveal_without_releasing_native_ownership() {
    let (_directory, service, mut events) = service(policy()).await;
    let owner = lease();
    service
        .try_submit(live_request("http://127.0.0.1:1", owner.clone()))
        .unwrap();
    let hidden = ready(&mut events).await;
    service
        .try_submit(live_request("http://127.0.0.1:1", owner.clone()))
        .unwrap();
    let sibling = ready(&mut events).await;

    assert!(!service.window_closing(&hidden.media_id, "main"));
    assert!(!service.window_closing(&hidden.media_id, &sibling.window_label));
    assert!(!service.window_closing(&sibling.media_id, &hidden.window_label));
    assert!(service.window_ready(&hidden.media_id, &hidden.window_label));
    assert!(service.window_closing(&hidden.media_id, &hidden.window_label));
    assert!(!service.window_ready(&hidden.media_id, &hidden.window_label));
    assert!(!service.window_closing(&hidden.media_id, &hidden.window_label));
    assert!(service
        .live_preview_url(&hidden.media_id, &hidden.window_label)
        .is_none());
    assert!(service.window_ready(&sibling.media_id, &sibling.window_label));
    assert!(owner.is_active(), "expiry cannot revoke sibling media");
    assert_eq!(
        service.0.state.lock().unwrap().items.len(),
        2,
        "hidden timeout does not acknowledge native absence"
    );

    service.window_destroyed(&hidden.window_label);
    service.window_destroyed(&sibling.window_label);
    idle(&service).await;
    assert!(!service.window_ready(&hidden.media_id, &hidden.window_label));
    assert!(!service.window_closing(&hidden.media_id, &hidden.window_label));
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn live_preview_owns_no_download_or_file_and_expires_before_releasing_its_window() {
    let (_directory, service, mut events) = service(policy()).await;
    let owner = lease();
    // An unreachable endpoint proves Ready does not wait for or download an endless stream.
    service
        .try_submit(live_request("http://127.0.0.1:1", owner))
        .unwrap();
    let preview = ready(&mut events).await;
    assert!(preview.live_url.is_some());
    assert!(preview.window_label.starts_with("plugin-preview-"));
    {
        let state = service.0.state.lock().unwrap();
        let item = state.items.get(&preview.media_id).unwrap();
        assert!(item.file.is_none());
        assert_eq!(item.bytes, 0);
        assert!(std::fs::read_dir(state.root.as_ref().unwrap())
            .unwrap()
            .next()
            .is_none());
    }
    assert!(service
        .read_range(&preview.media_id, &preview.window_label, None, false)
        .await
        .is_err());
    tokio::time::pause();
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(14)).await;
    assert!(events.try_recv().is_err());
    assert!(service.is_window_active(&preview.media_id, &preview.window_label));
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    let MediaEvent::Close { window_label } = events.recv().await.unwrap() else {
        panic!("expected live expiry")
    };
    assert_eq!(window_label, preview.window_label);
    assert!(!service.is_window_active(&preview.media_id, &preview.window_label));
    assert!(
        service.has_owned_work(),
        "native absence must be acknowledged before releasing ownership"
    );
    service.window_destroyed(&window_label);
    tokio::time::resume();
    idle(&service).await;
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn live_previews_share_window_bounds_and_revocation_cancels_queued_and_visible_work() {
    let (_directory, service, mut events) = service(policy()).await;
    let owner = lease();
    let mut previews = Vec::new();
    for _ in 0..MAX_WINDOWS {
        service
            .try_submit(live_request("http://127.0.0.1:1", owner.clone()))
            .unwrap();
        previews.push(ready(&mut events).await);
    }
    service
        .try_submit(live_request("http://127.0.0.1:1", owner.clone()))
        .unwrap();
    tokio::task::yield_now().await;
    assert!(
        events.try_recv().is_err(),
        "ninth live window must remain queued"
    );
    owner.revoke();
    for preview in &previews {
        assert!(!service.is_window_active(&preview.media_id, &preview.window_label));
    }
    for _ in 0..MAX_WINDOWS {
        let event = timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        let MediaEvent::Close { window_label } = event else {
            panic!("revoked queued preview must never become Ready")
        };
        service.window_destroyed(&window_label);
    }
    idle(&service).await;
    assert!(events.try_recv().is_err());
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn saturated_live_preview_expires_in_queue_and_never_replays_when_capacity_returns() {
    let (_directory, service, mut events) = service(policy()).await;
    let visible_owner = lease();
    let mut previews = Vec::new();
    for _ in 0..MAX_WINDOWS {
        service
            .try_submit(live_request("http://127.0.0.1:1", visible_owner.clone()))
            .unwrap();
        previews.push(ready(&mut events).await);
    }
    let queued_owner = lease();
    service
        .try_submit(live_request("http://127.0.0.1:1", queued_owner.clone()))
        .unwrap();
    tokio::task::yield_now().await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(4)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        service.0.state.lock().unwrap().items.len(),
        MAX_WINDOWS,
        "expired live queue entry released"
    );
    assert!(
        queued_owner.is_active(),
        "expiry must not revoke the worker or sibling requests"
    );
    let first = previews.remove(0);
    service.window_destroyed(&first.window_label);
    tokio::task::yield_now().await;
    assert!(
        events.try_recv().is_err(),
        "freeing capacity cannot replay an expired start"
    );
    tokio::time::resume();
    visible_owner.revoke();
    for _ in &previews {
        let MediaEvent::Close { window_label } = events.recv().await.unwrap() else {
            panic!("only remaining visible windows close")
        };
        service.window_destroyed(&window_label);
    }
    idle(&service).await;
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn expired_motion_cannot_start_even_when_a_native_window_is_available() {
    let (_directory, service, mut events) = service(policy()).await;
    let mut expired = live_request("http://127.0.0.1:1", lease());
    expired.created -= Duration::from_secs(3);
    service.try_submit(expired).unwrap();
    assert!(!service.has_owned_work());
    assert!(events.try_recv().is_err());

    // A fresh event from a different camera can still start immediately.
    service
        .try_submit(live_request("http://127.0.0.1:1", lease()))
        .unwrap();
    let fresh = ready(&mut events).await;
    service.window_destroyed(&fresh.window_label);
    idle(&service).await;
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn live_window_queue_keeps_the_original_runtime_delivery_deadline() {
    let (_directory, service, mut events) = service(policy()).await;
    let owner = lease();
    let mut previews = Vec::new();
    for _ in 0..MAX_WINDOWS {
        service
            .try_submit(live_request("http://127.0.0.1:1", owner.clone()))
            .unwrap();
        previews.push(ready(&mut events).await);
    }
    tokio::time::pause();
    let mut queued = live_request("http://127.0.0.1:1", owner);
    // Two seconds were already spent awaiting runtime dispatch. Only one
    // second remains; entering the native queue cannot renew that budget.
    queued.created -= Duration::from_secs(2);
    service.try_submit(queued).unwrap();
    tokio::task::yield_now().await;
    assert_eq!(service.0.state.lock().unwrap().items.len(), MAX_WINDOWS + 1);
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    assert_eq!(service.0.state.lock().unwrap().items.len(), MAX_WINDOWS);
    assert!(events.try_recv().is_err());
    for preview in previews {
        service.window_destroyed(&preview.window_label);
    }
    tokio::time::resume();
    idle(&service).await;
    assert!(events.try_recv().is_err(), "expired motion cannot replay");
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn delayed_live_task_cannot_reserve_available_capacity_after_motion_expires() {
    let (_directory, service, mut events) = service(policy()).await;
    tokio::time::pause();
    service
        .try_submit(live_request("http://127.0.0.1:1", lease()))
        .unwrap();
    // The worker task has not run yet. A delayed first poll must not turn an
    // expired motion into Ready merely because capacity is immediately free.
    tokio::time::advance(Duration::from_secs(3)).await;
    tokio::task::yield_now().await;
    assert!(!service.has_owned_work());
    assert!(events.try_recv().is_err());
    tokio::time::resume();
    service.shutdown().await.unwrap();
}
