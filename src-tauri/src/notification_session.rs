//! Bind transport events to the connection that produced them. The frontend owns
//! the opaque token, so it can reject an already-queued event after switching.
use serde::Serialize;
use serde_json::Value;
use std::ops::Deref;
use std::sync::Arc;
use tauri::Emitter;

#[derive(Clone)]
pub(crate) struct NotificationSession(Arc<str>);

fn is_scoped_event(event: &str) -> bool {
    matches!(
        event,
        "mqtt-notification"
            | "mqtt-notification-clear"
            | "notification"
            | "mqtt-state-update"
            | "mqtt-connection-status"
    )
}

impl NotificationSession {
    pub(crate) fn new(session: String) -> Result<Self, String> {
        // Validate before stopping or replacing the current connection. This is
        // an identity token, not broker data or a URL; preserve it byte-for-byte.
        if session.is_empty()
            || session.len() > 128
            || !session.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err("notificationSession must contain 1–128 printable ASCII bytes".into());
        }
        Ok(Self(session.into()))
    }

    pub(crate) fn bind(&self, payload: impl Serialize) -> Result<Value, serde_json::Error> {
        let Value::Object(mut fields) = serde_json::to_value(payload)? else {
            return Err(serde::ser::Error::custom(
                "notification event must be an object",
            ));
        };
        // Always overwrite broker-provided data. A delayed old producer must
        // never be able to claim the replacement client's notification session.
        fields.insert(
            "notification_session".into(),
            Value::String(self.0.to_string()),
        );
        Ok(Value::Object(fields))
    }

    fn event_payload(
        &self,
        event: &str,
        payload: impl Serialize,
    ) -> Result<Value, serde_json::Error> {
        let payload = serde_json::to_value(payload)?;
        if event == "mqtt-connection-status" {
            let Value::Bool(connected) = payload else {
                return Err(serde::ser::Error::custom(
                    "connection status must be a boolean",
                ));
            };
            self.bind(serde_json::json!({ "connected": connected }))
        } else {
            self.bind(payload)
        }
    }
}

/// A clone captures the original session permanently. There is intentionally no
/// global/current-session lookup at emission time: that would bless old events.
#[derive(Clone)]
pub(crate) struct SessionAppHandle {
    app: tauri::AppHandle,
    notification_session: NotificationSession,
}

impl SessionAppHandle {
    pub(crate) fn new(app: tauri::AppHandle, notification_session: NotificationSession) -> Self {
        Self {
            app,
            notification_session,
        }
    }

    pub(crate) fn emit<S: Serialize + Clone>(&self, event: &str, payload: S) -> tauri::Result<()> {
        if is_scoped_event(event) {
            self.app.emit(
                event,
                self.notification_session.event_payload(event, payload)?,
            )
        } else {
            // Control events keep their existing payloads. OS notification
            // calls also use the raw handle.
            self.app.emit(event, payload)
        }
    }

    pub(crate) fn bind(&self, payload: impl Serialize) -> Result<Value, serde_json::Error> {
        self.notification_session.bind(payload)
    }
}

impl Deref for SessionAppHandle {
    type Target = tauri::AppHandle;

    fn deref(&self) -> &Self::Target {
        &self.app
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mqtt::MqttNotification;
    use serde_json::json;

    #[test]
    fn old_producer_retains_its_session_after_replacement() {
        let original = NotificationSession::new("first-connection".into()).unwrap();
        let queued = original.clone();
        let replacement = NotificationSession::new("replacement-connection".into()).unwrap();
        drop(original);

        for payload in [
            json!({ "id": "victron-platform-0-2", "title": "Alarm" }),
            json!({ "id": "victron-platform-0-2" }),
        ] {
            assert_eq!(
                queued.bind(&payload).unwrap()["notification_session"],
                "first-connection"
            );
            assert_eq!(
                replacement.bind(&payload).unwrap()["notification_session"],
                "replacement-connection"
            );
        }
    }

    #[test]
    fn trusted_session_overwrites_spoofed_upsert_and_clear() {
        let session = NotificationSession::new("trusted-connection".into()).unwrap();
        for payload in [
            json!({ "id": "alarm", "source": "victron", "title": "Alarm",
                    "notification_session": "attacker-replacement" }),
            json!({ "id": "alarm", "notification_session": "attacker-replacement" }),
        ] {
            let bound = session.bind(&payload).unwrap();
            assert_eq!(bound["notification_session"], "trusted-connection");
            assert_eq!(bound["id"], payload["id"]);
        }
    }

    #[test]
    fn controller_notification_cannot_deserialize_an_authoritative_session() {
        let notification: MqttNotification = serde_json::from_value(json!({
            "id": "alarm", "level": "alarm", "title": "Alarm",
            "notification_session": "replacement-connection"
        }))
        .unwrap();
        let session = NotificationSession::new("original-connection".into()).unwrap();
        let bound = session.bind(notification).unwrap();
        assert_eq!(bound["notification_session"], "original-connection");
        assert_eq!(bound["title"], "Alarm");
    }

    #[test]
    fn queued_old_status_and_state_cannot_claim_the_replacement_session() {
        let old = NotificationSession::new("mqtt-old".into()).unwrap();
        let queued = old.clone();
        let replacement = NotificationSession::new("igw-new".into()).unwrap();
        drop(old);
        for connected in [true, false] {
            assert_eq!(
                queued
                    .event_payload("mqtt-connection-status", connected)
                    .unwrap(),
                json!({ "connected": connected, "notification_session": "mqtt-old" })
            );
            assert_eq!(
                replacement
                    .event_payload("mqtt-connection-status", connected)
                    .unwrap(),
                json!({ "connected": connected, "notification_session": "igw-new" })
            );
        }
        let state = json!({ "solar_total": 123.0, "notification_session": "igw-new" });
        let queued_state = queued.event_payload("mqtt-state-update", &state).unwrap();
        assert_eq!(queued_state["solar_total"], 123.0);
        assert_eq!(queued_state["notification_session"], "mqtt-old");
        assert_eq!(
            replacement
                .event_payload("mqtt-state-update", state)
                .unwrap()["notification_session"],
            "igw-new"
        );
        assert!(queued
            .event_payload(
                "mqtt-connection-status",
                json!({ "connected": true, "notification_session": "igw-new" })
            )
            .is_err());
    }

    #[test]
    fn generic_alert_history_is_scoped_without_changing_its_contents() {
        assert!(is_scoped_event("notification"));
        let old = NotificationSession::new("mqtt-old".into()).unwrap();
        let queued = old.clone();
        let current = NotificationSession::new("mqtt-new".into()).unwrap();
        drop(old);
        let alert = json!({
            "title": "High Consumption", "body": "Consumption: 5 kW",
            "notification_session": "mqtt-new"
        });
        let previous = queued.event_payload("notification", &alert).unwrap();
        assert_eq!(previous["notification_session"], "mqtt-old");
        assert_eq!(previous["title"], alert["title"]);
        assert_eq!(previous["body"], alert["body"]);
        assert_eq!(
            current.event_payload("notification", &alert).unwrap(),
            alert
        );
        assert!(!is_scoped_event("setpoint-override-update"));
    }

    #[test]
    fn malformed_or_unbounded_session_and_nonobject_payload_fail_closed() {
        for value in [
            "".to_string(),
            " ".into(),
            "token\n".into(),
            "x".repeat(129),
            "токен".into(),
        ] {
            assert!(NotificationSession::new(value).is_err());
        }
        let session =
            NotificationSession::new("9f70a57a-78e5-4d29-991c-e545c314659a".into()).unwrap();
        assert!(session.bind(json!(null)).is_err());
        assert!(session.bind(json!(["alarm"])).is_err());
        assert!(NotificationSession::new("x".repeat(128)).is_ok());
    }
}
