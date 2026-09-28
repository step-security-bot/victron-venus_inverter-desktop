//! Shared bounded transport and pure validation for independent camera workers.
pub mod exclusions;
pub mod live_urls;
pub mod network;

use serde_json::Value;
use std::{collections::BTreeMap, net::IpAddr, time::Instant};
use url::Url;

pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024;
pub const MAX_CONFIGURATION_BYTES: usize = 32 * 1024;
pub const MAX_CAMERAS: usize = 32;
pub const MAX_CACHE_ENTRIES: usize = 512;

// Deliberately no Debug: credentials never enter diagnostics.
pub struct Broker {
    pub provider: &'static str,
    pub title: &'static str,
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub topics: Vec<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

pub trait Provider {
    fn frames(
        &mut self,
        topic: &str,
        payload: &[u8],
        retained: bool,
        now: Instant,
        unix_seconds: u64,
    ) -> Vec<Value>;
}

pub fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.:-".contains(&c))
}

pub fn valid_identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.trim() == value
        && !value
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '+' | '#'))
}

pub fn validate_broker(broker: &Broker) -> Result<(), &'static str> {
    let host = &broker.host;
    let hostname = !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        });
    if (!hostname && host.parse::<IpAddr>().is_err()) || broker.port == 0 {
        return Err("invalid broker address");
    }
    for secret in [&broker.username, &broker.password].into_iter().flatten() {
        if secret.len() > 1024 || secret.chars().any(char::is_control) {
            return Err("invalid broker credentials");
        }
    }
    if broker.password.as_deref().is_some_and(|s| !s.is_empty())
        && broker.username.as_deref().is_none_or(str::is_empty)
    {
        return Err("broker password requires a username");
    }
    Ok(())
}

/// Only provider-specific structural patterns are accepted, never a global '#'.
pub fn topics(value: &str, provider: &str) -> Result<Vec<String>, &'static str> {
    if value.len() > 4096 || value.chars().any(char::is_control) {
        return Err("invalid camera topics");
    }
    let mut result = Vec::new();
    for topic in value.split(';').map(str::trim) {
        let parts: Vec<_> = topic.split('/').collect();
        let identifier = |s: &str| s == "+" || valid_identity(s);
        let valid = match (provider, parts.as_slice()) {
            ("kerberos", ["kerberos", "agent" | "hub", id]) => identifier(id),
            ("ring", ["ring", location, "camera", device, event, "state"]) => {
                identifier(location)
                    && identifier(device)
                    && matches!(*event, "motion" | "ding" | "+")
            }
            _ => false,
        };
        if !valid || topic.len() > 512 {
            return Err("invalid camera topics");
        }
        if !result.iter().any(|existing| existing == topic) {
            result.push(topic.to_owned());
        }
        if result.len() > 16 {
            return Err("too many camera topics");
        }
    }
    if result.is_empty() {
        return Err("camera topic required");
    }
    Ok(result)
}

pub fn topic_matches(filter: &str, topic: &str) -> bool {
    let mut actual = topic.split('/');
    for part in filter.split('/') {
        let Some(segment) = actual.next() else {
            return false;
        };
        if segment.is_empty() || (part != "+" && part != segment) {
            return false;
        }
    }
    actual.next().is_none()
}

pub fn mapping(
    value: Option<&str>,
    max_bytes: usize,
) -> Result<BTreeMap<String, String>, &'static str> {
    let Some(value) = value.filter(|s| !s.trim().is_empty()) else {
        return Ok(BTreeMap::new());
    };
    if value.len() > max_bytes {
        return Err("camera mapping too large");
    }
    let result: BTreeMap<String, String> =
        serde_json::from_str(value).map_err(|_| "invalid camera mapping")?;
    if result.len() > MAX_CAMERAS {
        return Err("too many configured cameras");
    }
    Ok(result)
}

pub fn labels(
    value: Option<&str>,
    valid_key: impl Fn(&str) -> bool,
) -> Result<BTreeMap<String, String>, &'static str> {
    let result = mapping(value, 8192)?;
    if result.iter().any(|(key, label)| {
        !valid_key(key)
            || label.trim().is_empty()
            || label.len() > 96
            || label.chars().any(char::is_control)
    }) {
        return Err("invalid camera labels");
    }
    Ok(result)
}

/// HTTP(S) URL validation without echoing credential-bearing input.
pub fn http_url(value: &str, allow_query: bool) -> Result<Url, &'static str> {
    if value.is_empty()
        || value.len() > 2048
        || value.trim() != value
        || value.chars().any(|c| c.is_control() || c == '\\')
    {
        return Err("invalid camera URL");
    }
    let (_, authority_path) = value.split_once("://").ok_or("invalid camera URL")?;
    let authority = authority_path
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let url = Url::parse(value).map_err(|_| "invalid camera URL")?;
    if authority.is_empty()
        || authority.contains('@')
        || !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || (!allow_query && url.query().is_some())
    {
        return Err("invalid camera URL");
    }
    Ok(url)
}

pub fn title(provider: &str, name: &str) -> String {
    let suffix = " camera motion detected";
    let mut title = format!("{provider} ");
    for c in name.chars() {
        if title.len() + c.len_utf8() + suffix.len() > 128 {
            break;
        }
        title.push(c);
    }
    title.push_str(suffix);
    title
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_are_provider_scoped_deduplicated_and_bounded() {
        assert_eq!(
            topics(
                "kerberos/agent/+; kerberos/hub/+;kerberos/agent/+",
                "kerberos"
            )
            .unwrap()
            .len(),
            2
        );
        assert!(topics("ring/+/camera/+/+/state", "ring").is_ok());
        for input in [
            "#",
            "kerberos/#",
            "frigate/events",
            "kerberos/agent/",
            "kerberos/agent/+;",
            "kerberos/agent/+\n",
        ] {
            assert!(topics(input, "kerberos").is_err());
        }
        for input in [
            "ring/#",
            "ring/+/camera/+/#",
            "ring/+/camera/+/battery/state",
            "ring/+/camera/+/motion/state/extra",
        ] {
            assert!(topics(input, "ring").is_err());
        }
        assert!(topics(
            &(0..17)
                .map(|n| format!("kerberos/agent/{n}"))
                .collect::<Vec<_>>()
                .join(";"),
            "kerberos"
        )
        .is_err());
        assert!(topic_matches(
            "ring/+/camera/+/+/state",
            "ring/site/camera/device/ding/state"
        ));
        assert!(!topic_matches("kerberos/agent/+", "kerberos/agent/a/extra"));
        assert!(!topic_matches("kerberos/agent/+", "kerberos/agent/"));
    }

    #[test]
    fn urls_and_unicode_titles_remain_bounded() {
        assert!(http_url("https://camera.test/view?fps=5", true).is_ok());
        for value in [
            "file:///a",
            "http://u:p@host/",
            "https://@host/",
            "https:///host",
            "http://host/#a",
            "http://host/\n",
        ] {
            assert!(http_url(value, true).is_err());
        }
        assert!(http_url("https://camera.test/?token=value", false).is_err());
        assert!(title("Kerberos", &"📷".repeat(128)).len() <= 128);
    }
}
