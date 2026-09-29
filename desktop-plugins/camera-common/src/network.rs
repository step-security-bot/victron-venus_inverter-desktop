use crate::{topic_matches, Broker, Provider, MAX_PAYLOAD_BYTES};
use inverter_worker_protocol::Output;
use rumqttc::{
    AsyncClient, Event, Incoming, MqttOptions, NetworkOptions, Outgoing, QoS, SubscribeFilter,
    SubscribeReasonCode, TlsConfiguration, Transport,
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

async fn status(
    output: &Output,
    provider: &str,
    title: &str,
    value: &str,
    tone: &str,
) -> Result<(), &'static str> {
    let id = format!("{provider}-mqtt");
    output
        .send(json!({
            "type":"contributions",
            "items":[{"kind":"status","id":"connection","title":title,"value":value,"tone":tone}],
            "presentation":[{"kind":"connection","id":id,"title":title,"connected":value=="Connected"}]
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

fn options(config: &Broker, transport: Transport) -> MqttOptions {
    let mut options = MqttOptions::builder(
        format!("inverter-{}-{}", config.provider, uuid::Uuid::new_v4()),
        (config.host.clone(), config.port),
    )
    .transport(transport)
    .keep_alive(30)
    .clean_session(true)
    .max_packet_size(MAX_PAYLOAD_BYTES + 1024, 8192)
    .request_channel_capacity(4)
    .max_request_batch(4)
    .read_batch_size(16)
    .inflight(4)
    .build();
    if let Some(username) = config.username.as_deref().filter(|value| !value.is_empty()) {
        if let Some(password) = config.password.as_deref().filter(|value| !value.is_empty()) {
            options.set_credentials(username, password.to_owned());
        } else {
            options.set_username(username);
        }
    }
    options
}

pub async fn run<P: Provider>(
    config: Broker,
    mut provider: P,
    output: &Output,
) -> Result<(), &'static str> {
    let transport = if config.tls {
        // Native trust loading can block; cancellation of the session never has
        // to wait for the OS certificate store to respond.
        tokio::task::spawn_blocking(tls_transport)
            .await
            .map_err(|_| "cannot initialize TLS")??
    } else {
        Transport::Tcp
    };
    let mut media_budget = NotificationBudget::default();
    let title = format!("{} MQTT", config.title);
    let mut notification_budget = NotificationBudget::default();
    let mut backoff = Duration::from_secs(1);
    loop {
        status(output, config.provider, &title, "Connecting", "neutral").await?;
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
                        .subscribe_many(
                            config
                                .topics
                                .iter()
                                .map(|topic| SubscribeFilter::new(topic.clone(), QoS::AtMostOnce)),
                        )
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
                        || ack.return_codes.len() != config.topics.len()
                        || ack
                            .return_codes
                            .iter()
                            .any(|code| *code != SubscribeReasonCode::Success(QoS::AtMostOnce))
                    {
                        break;
                    }
                    connected = true;
                    backoff = Duration::from_secs(1);
                    status(output, config.provider, &title, "Connected", "success").await?;
                }
                Ok(Event::Incoming(Incoming::Publish(message))) if connected => {
                    let Ok(topic) = std::str::from_utf8(&message.topic) else {
                        continue;
                    };
                    if !config
                        .topics
                        .iter()
                        .any(|filter| topic_matches(filter, topic))
                    {
                        continue;
                    }
                    let unix_seconds = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let now = Instant::now();
                    for frame in
                        provider.frames(topic, &message.payload, message.retain, now, unix_seconds)
                    {
                        // Independent host notification/media budgets permit both outputs for
                        // an admitted event. Pipe pressure never blocks MQTT polling.
                        let budget = if frame["type"] == "notification" {
                            &mut notification_budget
                        } else {
                            &mut media_budget
                        };
                        if budget.take(now) {
                            if matches!(frame["type"].as_str(), Some("live_view" | "mqtt_live")) {
                                // A camera preview must not become fresh again
                                // after waiting behind a blocked host pipe.
                                let _ =
                                    output.try_send_before(frame, now + Duration::from_secs(3))?;
                            } else {
                                let _ = output.try_send(frame)?;
                            }
                        }
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
        status(output, config.provider, &title, "Disconnected", "warning").await?;
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
