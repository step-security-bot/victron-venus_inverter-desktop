use inverter_camera_common::{
    topics, valid_token, validate_broker, Broker, MAX_CONFIGURATION_BYTES,
};
use serde::{Deserialize, Serialize};

// Deliberately no Debug: configuration includes credentials and private URLs.
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
    #[serde(default = "default_topics")]
    pub mqtt_topics: String,
    #[serde(default)]
    pub camera_labels: Option<String>,
    #[serde(default)]
    pub mqtt_live_endpoints: Option<String>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Secrets {
    #[serde(default)]
    pub mqtt_username: Option<String>,
    #[serde(default)]
    pub mqtt_password: Option<String>,
    #[serde(default)]
    pub camera_live_urls: Option<String>,
}

fn default_port() -> u16 {
    1883
}
fn default_topics() -> String {
    "kerberos/agent/+;kerberos/hub/+".into()
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
        self.broker()?;
        self.live_urls()?;
        inverter_camera_common::labels(
            self.values.camera_labels.as_deref(),
            inverter_camera_common::valid_identity,
        )?;
        Ok(())
    }
    pub fn broker(&self) -> Result<Broker, &'static str> {
        let mut subscribed = topics(&self.values.mqtt_topics, "kerberos")?;
        subscribed.extend(self.mqtt_live_urls()?.topics());
        if subscribed
            .iter()
            .map(|topic| topic.len() + 3)
            .sum::<usize>()
            > 8000
        {
            return Err("camera subscriptions exceed packet limit");
        }
        let broker = Broker {
            provider: "kerberos",
            title: "Kerberos",
            host: self.values.mqtt_host.clone(),
            port: self.values.mqtt_port,
            tls: self.values.mqtt_tls,
            topics: subscribed,
            username: self.secrets.mqtt_username.clone(),
            password: self.secrets.mqtt_password.clone(),
        };
        validate_broker(&broker)?;
        Ok(broker)
    }
    pub fn mqtt_live_urls(
        &self,
    ) -> Result<inverter_camera_common::live_urls::CameraLiveUrls, &'static str> {
        inverter_camera_common::live_urls::CameraLiveUrls::parse(
            self.values.mqtt_live_endpoints.as_deref(),
        )
    }
    pub fn live_urls(&self) -> Result<std::collections::BTreeMap<String, String>, &'static str> {
        let mapping =
            inverter_camera_common::mapping(self.secrets.camera_live_urls.as_deref(), 16384)?;
        for (id, value) in &mapping {
            if !inverter_camera_common::valid_identity(id) {
                return Err("invalid live camera identity");
            }
            // Fragments belong to the configured external page, never an MQTT payload.
            if value.len() > 2048
                || value.trim() != value
                || value.chars().any(char::is_control)
                || value.contains('\\')
            {
                return Err("invalid live camera URL");
            }
            inverter_camera_common::http_url(value.split('#').next().unwrap_or_default(), true)?;
        }
        Ok(mapping)
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
    fn default_configuration_and_broker_secrets_are_separate() {
        let config: Configuration = serde_json::from_value(valid()).unwrap();
        config.validate().unwrap();
        assert_eq!(config.values.mqtt_port, 1883);
        assert_eq!(config.broker().unwrap().topics.len(), 2);
        assert!(!config.values.mqtt_tls);
        let mut value = valid();
        value["values"]["mqtt_password"] = json!("not-public");
        assert!(serde_json::from_value::<Configuration>(value).is_err());
    }
    #[test]
    fn invalid_settings_fail_before_startup_without_echoing_secrets() {
        for (field, bad) in [
            ("mqtt_host", json!("http://host")),
            ("mqtt_host", json!("x".repeat(254))),
            ("mqtt_port", json!(0)),
            ("mqtt_tls", json!("false")),
            ("mqtt_topics", json!("#")),
            ("camera_labels", json!("not-json")),
        ] {
            let mut value = valid();
            value["values"][field] = bad;
            assert!(serde_json::from_value::<Configuration>(value)
                .ok()
                .is_none_or(|c| c.validate().is_err()));
        }
        for secret in [
            json!({"mqtt_password":"private-secret"}),
            json!({"mqtt_username":"u","mqtt_password":"x".repeat(1025)}),
            json!({"mqtt_username":"u\n"}),
        ] {
            let mut value = valid();
            value["secrets"] = secret;
            let c: Configuration = serde_json::from_value(value).unwrap();
            let error = c.validate().unwrap_err();
            assert!(!error.contains("private-secret"));
        }
        for host in ["127.0.0.1", "::1", "camera.example", "broker-1"] {
            let mut value = valid();
            value["values"]["mqtt_host"] = json!(host);
            serde_json::from_value::<Configuration>(value)
                .unwrap()
                .validate()
                .unwrap();
        }
    }
}
