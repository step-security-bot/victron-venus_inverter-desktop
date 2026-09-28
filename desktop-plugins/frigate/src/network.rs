use crate::{
    config::Configuration,
    frigate::{ClipEvents, MotionEvents, MAX_PAYLOAD_BYTES},
    wire::Output,
};
use rumqttc::{
    AsyncClient, Event, Incoming, MqttOptions, NetworkOptions, Outgoing, QoS, SubscribeReasonCode,
    TlsConfiguration, Transport,
};
use serde_json::json;
use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Default)]
struct NotificationBudget {
    window: Option<Instant>,
    count: usize,
}

impl NotificationBudget {
    fn take(&mut self, now: Instant) -> bool {
        if self
            .window
            .is_none_or(|start| now.saturating_duration_since(start) >= Duration::from_secs(60))
        {
            self.window = Some(now);
            self.count = 0;
        }
        if self.count >= 30 {
            return false;
        }
        self.count += 1;
        true
    }
}

async fn status(output: &Output, value: &str, tone: &str) -> Result<(), &'static str> {
    output
        .send(json!({
            "type":"contributions",
            "items":[{"kind":"status","id":"connection","title":"Frigate MQTT","value":value,"tone":tone}],
            "presentation":[{"kind":"connection","id":"frigate-mqtt","title":"Frigate MQTT","connected":value=="Connected"}]
        }))
        .await
}

fn tls_transport() -> Result<Transport, &'static str> {
    let certificates = rustls_native_certs::load_native_certs();
    let mut roots = rustls::RootCertStore::empty();
    for certificate in certificates.certs {
        roots
            .add(certificate)
            .map_err(|_| "cannot load trusted TLS certificates")?;
    }
    if roots.is_empty() {
        return Err("no trusted TLS certificates available");
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Transport::tls_with_config(TlsConfiguration::Rustls(
        Arc::new(config),
    )))
}

fn options(config: &Configuration, transport: Transport) -> MqttOptions {
    let mut options = MqttOptions::builder(
        format!("inverter-frigate-{}", uuid::Uuid::new_v4()),
        (config.values.mqtt_host.clone(), config.values.mqtt_port),
    )
    .transport(transport)
    .keep_alive(30)
    .clean_session(true)
    .max_packet_size(MAX_PAYLOAD_BYTES + 1024, 4096)
    .request_channel_capacity(4)
    .max_request_batch(4)
    .read_batch_size(16)
    .inflight(4)
    .build();
    if let Some(username) = config
        .secrets
        .mqtt_username
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        if let Some(password) = config
            .secrets
            .mqtt_password
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            options.set_credentials(username, password.to_owned());
        } else {
            options.set_username(username);
        }
    }
    options
}

pub async fn run(
    config: Configuration,
    output: &Output,
    supports_http_video: bool,
    supports_http_live: bool,
) -> Result<(), &'static str> {
    let base = if supports_http_video {
        crate::media::base_url(config.values.frigate_base_url.as_deref())?
    } else {
        None
    };
    let transport = if config.values.mqtt_tls {
        // Native trust loading can block; cancellation of the session never has
        // to wait for the OS certificate store to respond.
        tokio::task::spawn_blocking(tls_transport)
            .await
            .map_err(|_| "cannot initialize TLS")??
    } else {
        Transport::Tcp
    };
    let excluded_cameras = config.excluded_cameras()?;
    let mut events = MotionEvents::excluding(excluded_cameras.clone());
    let mut clips = ClipEvents::excluding(excluded_cameras);
    let mut notification_budget = NotificationBudget::default();
    let mut backoff = Duration::from_secs(1);
    loop {
        status(output, "Connecting", "neutral").await?;
        let (client, mut eventloop) = AsyncClient::builder(options(&config, transport.clone()))
            .capacity(4)
            .build();
        let mut network = NetworkOptions::new();
        network.set_connection_timeout(10);
        eventloop.set_network_options(network);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut subscription = None;
        let mut connected = false;
        loop {
            let event = if connected {
                eventloop.poll().await
            } else {
                match tokio::time::timeout_at(deadline, eventloop.poll()).await {
                    Ok(event) => event,
                    Err(_) => break,
                }
            };
            match event {
                Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                    if client
                        .subscribe(&config.values.mqtt_topic, QoS::AtMostOnce)
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(Event::Outgoing(Outgoing::Subscribe(pkid))) => {
                    subscription = Some(pkid);
                }
                Ok(Event::Incoming(Incoming::SubAck(ack))) => {
                    if connected
                        || subscription != Some(ack.pkid)
                        || ack.return_codes != [SubscribeReasonCode::Success(QoS::AtMostOnce)]
                    {
                        break;
                    }
                    connected = true;
                    backoff = Duration::from_secs(1);
                    status(output, "Connected", "success").await?;
                }
                Ok(Event::Incoming(Incoming::Publish(message)))
                    if connected && message.topic == config.values.mqtt_topic =>
                {
                    let unix_seconds = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs_f64();
                    let now = Instant::now();
                    let frame = if let Some(motion) =
                        events.parse(&message.payload, message.retain, now, unix_seconds)
                    {
                        let live = base
                            .as_ref()
                            .filter(|_| supports_http_live)
                            .and_then(|base| crate::media::live_url(base, &motion.camera));
                        Some(match live {
                            Some(url) => {
                                json!({"type":"http_live","id":motion.id,"url":url,"title":motion.title})
                            }
                            None => {
                                json!({"type":"notification","id":motion.id,"title":motion.title,"body":"Motion started"})
                            }
                        })
                    } else if supports_http_live {
                        // A live host shows the scene immediately. Do not open
                        // another delayed window when that recording ends.
                        None
                    } else {
                        clips.parse(&message.payload, message.retain, now, base.as_ref())
                            .map(|clip| json!({"type":"http_video","id":clip.id,"url":clip.url,"title":clip.title}))
                    };
                    // One shared budget preserves the existing frame bound.
                    // The bounded queue drops events while the host is slow;
                    // neither video fetching nor pipe backpressure blocks MQTT.
                    if let Some(frame) = frame.filter(|_| notification_budget.take(now)) {
                        let _ = output.try_send(frame)?;
                    }
                }
                Err(_) => break,
                _ => {}
            }
        }
        // Dropping the event loop closes the old transport and discards queued
        // packets. Each retry opens a clean session and subscribes again.
        drop(client);
        drop(eventloop);
        status(output, "Disconnected", "warning").await?;
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broker_bursts_cannot_exceed_host_notification_budget() {
        let now = Instant::now();
        let mut budget = NotificationBudget::default();
        for _ in 0..30 {
            assert!(budget.take(now));
        }
        for _ in 0..512 {
            assert!(!budget.take(now));
        }
        assert!(!budget.take(now + Duration::from_secs(59)));
        assert!(budget.take(now + Duration::from_secs(60)));
    }
}
