//! GUIv2 notification snapshots use the same banner semantics as direct MQTT.
use crate::mqtt::{MqttNotification, PlatformNotifSlot};
use chrono::{TimeZone, Utc};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Default)]
pub(super) struct GatewayNotifications {
    visible: HashMap<String, MqttNotification>,
}

#[derive(Default)]
pub(super) struct Changes {
    pub upsert: Vec<MqttNotification>,
    pub clear: Vec<String>,
}

fn slot_path(path: &str) -> Option<(u32, u32, &str)> {
    let mut parts = path.split('/');
    let instance = parts.next()?.parse().ok()?;
    if parts.next()? != "Notifications" {
        return None;
    }
    let slot = parts
        .next()?
        .parse::<u32>()
        .ok()
        .filter(|slot| *slot <= 20)?;
    let field = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    Some((instance, slot, field))
}

impl GatewayNotifications {
    pub fn update(&mut self, platform: &HashMap<String, Value>) -> Changes {
        let mut slots: HashMap<(u32, u32), HashMap<String, Value>> = HashMap::new();
        for (path, value) in platform {
            if let Some((instance, slot, field)) = slot_path(path) {
                slots
                    .entry((instance, slot))
                    .or_default()
                    .insert(field.into(), value.clone());
            }
        }
        let mut next = HashMap::new();
        for ((instance, slot), fields) in slots {
            // Do not manufacture a new timestamp on every poll of incomplete data.
            // Acknowledged is required: missing status is not a new unacked alarm.
            if !fields
                .get("Acknowledged")
                .is_some_and(|v| v.is_boolean() || matches!(v.as_i64(), Some(0 | 1)))
                || !fields.get("DateTime").is_some_and(|v| {
                    v.as_i64()
                        .is_some_and(|t| t > 0 && Utc.timestamp_opt(t, 0).single().is_some())
                })
            {
                continue;
            }
            let notification = PlatformNotifSlot::from_gateway_fields(instance, slot, &fields);
            if let Some(notification) = notification.to_notification() {
                next.insert(notification.id.clone(), notification);
            }
        }
        let mut changes = Changes::default();
        for (id, notification) in &next {
            if self.visible.get(id) != Some(notification) {
                changes.upsert.push(notification.clone());
            }
        }
        changes.clear = self
            .visible
            .keys()
            .filter(|id| !next.contains_key(*id))
            .cloned()
            .collect();
        changes
            .upsert
            .sort_by(|a, b| a.ts.cmp(&b.ts).then(a.id.cmp(&b.id)));
        changes.clear.sort();
        self.visible = next;
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::GatewaySnapshot;
    use serde_json::json;

    fn fields(instance: u32, slot: u32, acknowledged: bool) -> HashMap<String, Value> {
        [
            ("Description", json!("Internal failure")),
            ("DeviceName", json!("JBD Battery Chain 1")),
            ("DateTime", json!(1_700_000_000)),
            ("Type", json!(1)),
            ("Active", json!(false)),
            ("Acknowledged", json!(acknowledged)),
            ("Silenced", json!(false)),
        ]
        .into_iter()
        .map(|(field, value)| (format!("{instance}/Notifications/{slot}/{field}"), value))
        .collect()
    }

    #[test]
    fn snapshot_deserialization_restores_three_inactive_unacknowledged_gui_alerts() {
        let mut platform = fields(0, 11, false);
        platform.extend(fields(0, 12, false));
        platform.extend(fields(0, 13, false));
        platform.insert(
            "0/Notifications/13/Description".into(),
            json!("Low battery voltage"),
        );
        platform.insert("0/Notifications/13/DeviceName".into(), json!("Quattro"));
        platform.extend(fields(0, 10, true));
        let snap: GatewaySnapshot = serde_json::from_value(json!({"platform": platform})).unwrap();
        let mut tracker = GatewayNotifications::default();
        let changes = tracker.update(&snap.platform);
        assert_eq!(changes.upsert.len(), 3);
        assert!(changes.clear.is_empty());
        assert_eq!(changes.upsert[2].title, "Low battery voltage");
        assert_eq!(changes.upsert[2].body, "Quattro");
        assert_eq!(changes.upsert[0].source, "victron");
        assert_eq!(changes.upsert[0].level, "alarm");
        assert!(
            tracker.update(&snap.platform).upsert.is_empty(),
            "2-second polls must not spam history"
        );
    }

    #[test]
    fn acknowledgement_and_removed_slots_clear_banners_once() {
        let mut tracker = GatewayNotifications::default();
        tracker.update(&fields(0, 1, false));
        let cleared = tracker.update(&fields(0, 1, true));
        assert_eq!(cleared.clear, ["victron-platform-0-1"]);
        assert!(cleared.upsert.is_empty());
        assert!(tracker.update(&fields(0, 1, true)).clear.is_empty());
        tracker.update(&fields(0, 2, false));
        assert_eq!(
            tracker.update(&HashMap::new()).clear,
            ["victron-platform-0-2"]
        );
    }

    #[test]
    fn recycled_slot_with_new_event_is_delivered_and_platform_instances_do_not_collide() {
        let mut tracker = GatewayNotifications::default();
        let mut snapshot = fields(0, 1, false);
        snapshot.extend(fields(1, 1, false));
        assert_eq!(tracker.update(&snapshot).upsert.len(), 2);
        snapshot.insert("0/Notifications/1/DateTime".into(), json!(1_700_000_001));
        let changes = tracker.update(&snapshot);
        assert_eq!(changes.upsert.len(), 1);
        assert_eq!(changes.upsert[0].id, "victron-platform-0-1");
    }

    #[test]
    fn silencing_does_not_acknowledge_or_hide_an_outstanding_banner() {
        let mut tracker = GatewayNotifications::default();
        let mut snapshot = fields(0, 1, false);
        snapshot.insert("0/Notifications/1/Silenced".into(), json!(true));
        snapshot.insert("0/Notifications/1/Type".into(), json!(0));
        assert_eq!(tracker.update(&snapshot).upsert[0].level, "warning");
        snapshot.insert("0/Notifications/1/Active".into(), json!(true));
        assert!(tracker.update(&snapshot).upsert.is_empty());
    }

    #[test]
    fn missing_old_gateway_field_and_malformed_slots_do_not_invent_alerts() {
        let snap: GatewaySnapshot = serde_json::from_value(json!({"system": {}})).unwrap();
        let mut tracker = GatewayNotifications::default();
        assert!(tracker.update(&snap.platform).upsert.is_empty());
        for missing in ["Description", "Acknowledged", "DateTime"] {
            let mut snapshot = fields(0, 1, false);
            snapshot.remove(&format!("0/Notifications/1/{missing}"));
            assert!(tracker.update(&snapshot).upsert.is_empty());
        }
        for path in [
            "0/Notifications/21/Description",
            "0/Notifications/1/Description/extra",
            "0/Alarms/1/Description",
            "bad/Notifications/1/Description",
        ] {
            assert!(slot_path(path).is_none());
        }
        let mut snapshot = fields(0, 1, false);
        snapshot.insert("0/Notifications/1/Acknowledged".into(), json!("unknown"));
        assert!(tracker.update(&snapshot).upsert.is_empty());
    }
}
