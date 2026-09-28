//! Bounded exact camera identities; exclusions never interpret MQTT wildcards.
use std::collections::BTreeSet;

pub fn parse(
    value: Option<&str>,
    valid_identity: fn(&str) -> bool,
) -> Result<BTreeSet<String>, &'static str> {
    let value = value.unwrap_or_default();
    if value.len() > 8192 {
        return Err("invalid excluded cameras");
    }
    if value.trim().is_empty() {
        return Ok(BTreeSet::new());
    }
    let ids: Vec<String> = serde_json::from_str(value).map_err(|_| "invalid excluded cameras")?;
    if ids.len() > 32
        || ids.iter().any(|id| {
            id.trim().is_empty()
                || id.len() > 128
                || id.chars().any(char::is_control)
                || !valid_identity(id)
        })
    {
        return Err("invalid excluded cameras");
    }
    Ok(ids.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_bounds_types_and_exact_identities() {
        for value in [None, Some(""), Some("  "), Some("[]")] {
            assert!(parse(value, |_| true).unwrap().is_empty());
        }
        let ids = parse(Some(r#"["front","Front","front","front-east"]"#), |_| true).unwrap();
        assert_eq!(ids.len(), 3);
        assert!(!ids.contains("FRONT"));
        for value in [
            json!({"front":true}).to_string(),
            json!([1]).to_string(),
            json!([""]).to_string(),
            json!(["  "]).to_string(),
            json!(["front\n"]).to_string(),
            json!(["é".repeat(65)]).to_string(),
            json!(vec!["front"; 33]).to_string(),
            " ".repeat(8193),
        ] {
            assert_eq!(
                parse(Some(&value), |_| true).unwrap_err(),
                "invalid excluded cameras"
            );
        }
        assert_eq!(
            parse(Some(&json!(["é".repeat(64)]).to_string()), |_| true)
                .unwrap()
                .len(),
            1
        );
        assert!(parse(Some(r#"["front"]"#), |_| false).is_err());
    }
}
