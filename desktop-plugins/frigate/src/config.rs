use serde::{Deserialize, Serialize};
use std::net::IpAddr;

pub const MAX_CONFIGURATION_BYTES: usize = 32 * 1024;

// No Debug implementation: broker credentials must never enter diagnostics.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    pub revision: String,
    pub values: Values,
    #[serde(default)]
    pub secrets: Secrets,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Values {
    pub mqtt_host: String,
    #[serde(default = "default_port")]
    pub mqtt_port: u16,
    #[serde(default)]
    pub mqtt_tls: bool,
    #[serde(default = "default_topic")]
    pub mqtt_topic: String,
    #[serde(default)]
    pub frigate_base_url: Option<String>,
    #[serde(default)]
    pub excluded_cameras: Option<String>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Secrets {
    #[serde(default)]
    pub mqtt_username: Option<String>,
    #[serde(default)]
    pub mqtt_password: Option<String>,
}

fn default_port() -> u16 {
    1883
}
fn default_topic() -> String {
    "frigate/events".into()
}

pub fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte))
}

impl Configuration {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !valid_token(&self.revision)
            || serde_json::to_vec(self)
                .map_err(|_| "invalid configuration")?
                .len()
                > MAX_CONFIGURATION_BYTES
        {
            return Err("invalid configuration");
        }
        let values = &self.values;
        crate::media::base_url(values.frigate_base_url.as_deref())?;
        self.excluded_cameras()?;
        let host = &values.mqtt_host;
        let hostname = !host.is_empty()
            && host.len() <= 253
            && host.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            });
        if (!hostname && host.parse::<IpAddr>().is_err()) || values.mqtt_port == 0 {
            return Err("invalid broker address");
        }
        let topic = &values.mqtt_topic;
        if topic.is_empty()
            || topic.len() > 256
            || topic.trim() != topic
            || topic
                .chars()
                .any(|ch| ch.is_control() || matches!(ch, '#' | '+' | ';'))
        {
            return Err("invalid camera topic");
        }
        for secret in [&self.secrets.mqtt_username, &self.secrets.mqtt_password]
            .into_iter()
            .flatten()
        {
            if secret.len() > 1024 || secret.chars().any(char::is_control) {
                return Err("invalid broker credentials");
            }
        }
        if self
            .secrets
            .mqtt_password
            .as_deref()
            .is_some_and(|s| !s.is_empty())
            && self
                .secrets
                .mqtt_username
                .as_deref()
                .is_none_or(str::is_empty)
        {
            return Err("broker password requires a username");
        }
        Ok(())
    }

    pub fn excluded_cameras(&self) -> Result<std::collections::BTreeSet<String>, &'static str> {
        crate::exclusions::parse(self.values.excluded_cameras.as_deref(), |_| true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    fn valid() -> Value {
        json!({"revision":"rev-1","values":{"mqtt_host":"localhost"},"secrets":{}})
    }
    #[test]
    fn defaults_and_secrets_are_separate() {
        let config: Configuration = serde_json::from_value(valid()).unwrap();
        config.validate().unwrap();
        assert_eq!(config.values.mqtt_port, 1883);
        assert_eq!(config.values.mqtt_topic, "frigate/events");
        assert!(!config.values.mqtt_tls);
        assert!(config.values.frigate_base_url.is_none());
        assert!(config.excluded_cameras().unwrap().is_empty());
        let mut value = valid();
        value["values"]["mqtt_password"] = json!("not-public");
        assert!(serde_json::from_value::<Configuration>(value).is_err());
    }
    #[test]
    fn validates_addresses_topics_types_and_secret_bounds() {
        for (field, values) in [
            (
                "mqtt_host",
                vec![
                    json!(""),
                    json!("http://host"),
                    json!("host/path"),
                    json!("user@host"),
                    json!("a".repeat(254)),
                ],
            ),
            (
                "mqtt_topic",
                vec![
                    json!(""),
                    json!("#"),
                    json!("frigate/+"),
                    json!("a;b"),
                    json!("a\n"),
                    json!("a".repeat(257)),
                ],
            ),
            ("mqtt_port", vec![json!(0), json!(65536), json!("1883")]),
            ("mqtt_tls", vec![json!("false")]),
            (
                "excluded_cameras",
                vec![json!(["front"]), json!("{}"), json!(r#"[""]"#)],
            ),
            (
                "frigate_base_url",
                vec![json!(7), json!(true), json!("https://user:password@host")],
            ),
        ] {
            for invalid in values {
                let mut value = valid();
                value["values"][field] = invalid;
                assert!(serde_json::from_value::<Configuration>(value)
                    .ok()
                    .is_none_or(|c| c.validate().is_err()));
            }
        }
        for host in ["127.0.0.1", "::1", "frigate.example", "broker-1"] {
            let mut value = valid();
            value["values"]["mqtt_host"] = json!(host);
            serde_json::from_value::<Configuration>(value)
                .unwrap()
                .validate()
                .unwrap();
        }
        for secret in [
            json!({"mqtt_password":"password"}),
            json!({"mqtt_username":"u", "mqtt_password":"x".repeat(1025)}),
            json!({"mqtt_username":"u\n"}),
        ] {
            let mut value = valid();
            value["secrets"] = secret;
            assert!(serde_json::from_value::<Configuration>(value)
                .unwrap()
                .validate()
                .is_err());
        }
    }
}
