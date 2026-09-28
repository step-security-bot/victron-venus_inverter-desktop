//! Separate std-only executable used by the worker supervisor's pipe integration tests.
use std::io::{self, BufRead, Write};
use std::time::Duration;

fn field<'a>(line: &'a str, name: &str) -> &'a str {
    let key = format!("\"{name}\":\"");
    line.split_once(&key)
        .and_then(|(_, tail)| tail.split_once('"'))
        .map_or("", |(value, _)| value)
}

fn emit(value: &str) {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{value}").unwrap();
    stdout.flush().unwrap();
}

fn contribute() {
    emit(
        r#"{"type":"contributions","items":[{"kind":"text","id":"greeting","title":"Fixture","text":"Separate executable"},{"kind":"action","id":"echo","title":"Echo","action_id":"echo","label":"Echo","params":{}},{"kind":"action","id":"hold","title":"Hold","action_id":"hold","label":"Hold","params":{}},{"kind":"action","id":"crash","title":"Crash","action_id":"crash","label":"Crash","params":{}},{"kind":"action","id":"cancel_count","title":"Cancellations","action_id":"cancel_count","label":"Count","params":{}}]}"#,
    );
}

fn contribute_numeric(revision: &str, value: i64, withdrawn: bool) {
    let input = if withdrawn {
        String::new()
    } else {
        format!(
            r#",{{"kind":"number_input","id":"numeric-input","title":"Temperature","action_id":"set-number","label":"Set temperature","unit":"°C","input_revision":"{revision}","value_scaled":{value},"min_scaled":-25,"max_scaled":25,"step_scaled":5,"decimal_places":1}}"#
        )
    };
    emit(&format!(
        r#"{{"type":"contributions","items":[{{"kind":"action","id":"numeric-control","title":"Fixture","action_id":"numeric-control","label":"Update","params":{{}}}}{input}]}}"#
    ));
}

fn integer_field<'a>(line: &'a str, name: &str) -> &'a str {
    let key = format!("\"{name}\":");
    let tail = line.split_once(&key).unwrap().1;
    &tail[..tail
        .find(|character: char| character != '-' && !character.is_ascii_digit())
        .unwrap()]
}

fn notification(id: &str) {
    emit(&format!(
        "{{\"type\":\"notification\",\"id\":\"{id}\",\"title\":\"Private camera title\",\"body\":\"Private camera body\"}}"
    ));
}

fn http_video(id: &str, title: &str, url: &str) {
    emit(&format!(
        "{{\"type\":\"http_video\",\"id\":\"{id}\",\"url\":\"{url}\",\"title\":\"{title}\"}}"
    ));
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| {
        let executable = std::env::current_exe().unwrap();
        let stem = executable.file_stem().unwrap().to_string_lossy();
        if stem.starts_with("configuration") || stem.starts_with("notifications") {
            stem.into_owned()
        } else {
            "normal".into()
        }
    });
    let mut cancellations = 0;
    let mut configuration_revision = String::new();
    let mut configuration_secret_matches = false;
    let mut notification_sequence = 0;
    let mut numeric_updates = 0;
    let mut numeric_writes = 0;
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = line.unwrap();
        match field(&line, "type") {
            "hello" => {
                if mode == "no_hello" {
                    std::thread::sleep(Duration::from_secs(60));
                    return;
                }
                let id = if mode == "bad_identity" {
                    "other.worker"
                } else {
                    field(&line, "plugin_id")
                };
                let version = if mode == "bad_version" { 999 } else { 1 };
                let api = field(&line, "host_api_version");
                if mode == "notifications_before_ready" {
                    notification("premature");
                }
                emit(&format!("{{\"type\":\"ready\",\"protocol_version\":{version},\"host_api_version\":\"{api}\",\"plugin_id\":\"{id}\"}}"));
                if mode == "configuration_early_video" {
                    http_video(
                        "early",
                        "Private camera",
                        "https://video.test/base/api/events/one/clip.mp4",
                    );
                }
                if mode == "configuration_early_notification" {
                    notification("before-configuration-ack");
                }
                if mode == "oversize" {
                    print!("{}", "x".repeat(70_000));
                    io::stdout().flush().unwrap();
                    std::thread::sleep(Duration::from_secs(60));
                    return;
                }
                if mode == "numeric" {
                    contribute_numeric("input-1", -15, false);
                } else if !mode.starts_with("configuration") || mode == "configuration_early_data" {
                    contribute();
                }
                if mode == "notifications" {
                    notification("motion-1");
                    notification("motion-1");
                    notification("motion-2");
                }
                if mode == "notifications_oversize" {
                    emit(&format!("{{\"type\":\"notification\",\"id\":\"large\",\"title\":\"{}\",\"body\":\"Body\"}}", "x".repeat(129)));
                }
                if mode == "notifications_flood" {
                    for index in 0..200 {
                        notification(&format!("flood-{index}"));
                    }
                }
                if mode == "crash_always" {
                    std::process::exit(17);
                }
                if mode == "stalled" {
                    std::thread::sleep(Duration::from_secs(60));
                    return;
                }
                if mode == "flood" {
                    for _ in 0..200 {
                        emit(r#"{"type":"event","name":"tick","data":{}}"#);
                    }
                }
                if mode == "stderr_flood" {
                    let mut stderr = io::stderr().lock();
                    for _ in 0..128 {
                        stderr.write_all(&[b's'; 4096]).unwrap();
                    }
                    stderr.flush().unwrap();
                }
            }
            "configuration" => {
                configuration_revision = field(&line, "revision").to_owned();
                configuration_secret_matches = field(&line, "token") == "fixture-secret";
                if mode == "configuration_no_ack" {
                    std::thread::sleep(Duration::from_secs(60));
                    return;
                }
                if mode == "configuration_delayed" {
                    std::thread::sleep(Duration::from_millis(150));
                }
                let revision = if mode == "configuration_wrong_ack" {
                    "wrong"
                } else {
                    &configuration_revision
                };
                let ack =
                    format!("{{\"type\":\"configuration_ready\",\"revision\":\"{revision}\"}}");
                emit(&ack);
                if mode == "configuration_duplicate_ack" {
                    emit(&ack);
                }
                contribute();
                if mode.starts_with("configuration_mqtt_live") {
                    for (id, camera, token) in [
                        ("mqtt-1", "front", "private_fixture"),
                        ("mqtt-2", "rear", "private_fixture"),
                        ("mqtt-3", "front", "rotated_fixture"),
                    ] {
                        let path_camera = if mode.ends_with("bad_url") { "other" } else { camera };
                        emit(&format!(
                            r#"{{"type":"mqtt_live","id":"{id}","title":"Camera","camera_id":"{camera}","url":"https://ha.invalid/api/camera_proxy_stream/camera.{path_camera}?token={token}"}}"#
                        ));
                    }
                }
                if mode.starts_with("configuration_mapped_live") {
                    for (id, camera) in [
                        ("mapped-1", "front"),
                        ("mapped-2", "rear"),
                        ("mapped-3", "front"),
                        ("mapped-4", "missing"),
                    ] {
                        emit(&format!(
                            r#"{{"type":"live_view","id":"{id}","title":"Private camera","live_view_id":"{camera}"}}"#
                        ));
                    }
                }
                if mode.starts_with("configuration_live") {
                    let url = if mode == "configuration_live_bad_url" {
                        "https://other.test/base/api/front?fps=2&height=360"
                    } else {
                        "https://video.test/base/api/front?fps=2&height=360"
                    };
                    emit(&format!(
                        r#"{{"type":"http_live","id":"live-1","url":"{url}","title":"Private camera"}}"#
                    ));
                }
                if mode.starts_with("configuration_video") {
                    let url = if mode.ends_with("_bad_url") {
                        "https://other.test/base/clip.mp4"
                    } else {
                        "https://video.test/base/api/events/one/clip.mp4"
                    };
                    if mode.starts_with("configuration_video_image") {
                        emit(&format!(
                            r#"{{"type":"http_video","id":"image-1","url":"{url}","title":"Private snapshot","media_kind":"jpeg"}}"#
                        ));
                    } else {
                        http_video("clip-1", "Private camera", url);
                    }
                }
                if mode == "configuration_notifications" {
                    notification("configured-motion");
                }
            }
            "action" => {
                let request_id = field(&line, "request_id");
                if mode == "numeric" {
                    let value = match field(&line, "action_id") {
                        "set-number" => {
                            numeric_writes += 1;
                            format!(
                                r#"{{"input_revision":"{}","value_scaled":{},"writes":{numeric_writes}}}"#,
                                field(&line, "input_revision"),
                                integer_field(&line, "value_scaled")
                            )
                        }
                        "numeric-control" => {
                            numeric_updates += 1;
                            match numeric_updates {
                                1 => contribute_numeric("input-1", -10, false),
                                2 => contribute_numeric("input-2", -10, false),
                                3 => contribute_numeric("input-2", -10, true),
                                _ => contribute_numeric("input-3", -10, false),
                            }
                            numeric_writes.to_string()
                        }
                        _ => panic!("unexpected numeric action"),
                    };
                    emit(&format!(
                        r#"{{"type":"action_result","request_id":"{request_id}","value":{value}}}"#
                    ));
                    continue;
                }
                if field(&line, "action_id") == "echo" {
                    if mode == "configuration_video" {
                        http_video(
                            "clip-1",
                            "Different camera",
                            "https://video.test/base/api/events/one/clip.mp4",
                        );
                        http_video(
                            "clip-2",
                            "Private camera",
                            "https://video.test/base/api/events/two/clip.mp4",
                        );
                    }
                    if mode == "configuration_video_burst" {
                        for index in 0..10 {
                            http_video(
                                &format!("clip-{index}"),
                                &format!("Camera {index}"),
                                "https://video.test/base/clip.mp4",
                            );
                        }
                    }
                    if mode == "notifications" {
                        notification("motion-1");
                        notification("motion-2");
                    }
                    if matches!(
                        mode.as_str(),
                        "notifications_actions" | "notifications_authorized"
                    ) {
                        for _ in 0..20 {
                            notification(&format!("motion-{notification_sequence}"));
                            notification_sequence += 1;
                        }
                    }
                }
                let value = match field(&line, "action_id") {
                    "hold" => continue,
                    "crash" => std::process::exit(23),
                    "cancel_count" => cancellations.to_string(),
                    _ => format!(
                        "{{\"ok\":true,\"pid\":{},\"inherited_environment\":{},\"configuration_revision\":\"{}\",\"configuration_secret_matches\":{}}}",
                        std::process::id(),
                        std::env::var_os("CARGO_MANIFEST_DIR").is_some(),
                        configuration_revision,
                        configuration_secret_matches
                    ),
                };
                emit(&format!("{{\"type\":\"action_result\",\"request_id\":\"{request_id}\",\"value\":{value}}}"));
            }
            "cancel" => {
                cancellations += 1;
                // A non-cooperative worker may send a result after cancellation.
                let request_id = field(&line, "request_id");
                emit(&format!("{{\"type\":\"action_result\",\"request_id\":\"{request_id}\",\"value\":\"late\"}}"));
            }
            "shutdown" => return,
            _ => {}
        }
    }
}
