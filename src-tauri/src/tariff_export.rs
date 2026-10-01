//! Export a tariff through a user-selected native document, not a WebView download.
use serde_json::Value;
use tauri_plugin_dialog::DialogExt;

fn serialize(plan: &Value) -> Result<String, String> {
    if !plan.is_object() || plan.get("version").and_then(Value::as_u64) != Some(2) {
        return Err("Invalid tariff export".into());
    }
    let content = serde_json::to_string_pretty(plan)
        .map_err(|_| "Could not serialize tariff export".to_owned())?;
    // Pretty JSON for all twelve seasons can exceed the compact import limit.
    // Keep a separate export bound large enough for every supported rate grid.
    if content.len() > 1_048_576 {
        return Err("Tariff export is too large".into());
    }
    Ok(content)
}

#[tauri::command]
pub(crate) async fn export_tariff(app: tauri::AppHandle, plan: Value) -> Result<bool, String> {
    let content = serialize(&plan)?;
    // Never block the WebView's main thread with a file dialog or disk I/O.
    // No destination path is accepted from JavaScript.
    tauri::async_runtime::spawn_blocking(move || {
        let Some(file) = app
            .dialog()
            .file()
            .set_file_name("electricity-tariff.json")
            .add_filter("JSON", &["json"])
            .blocking_save_file()
        else {
            return Ok(false);
        };
        crate::config_file_io::write(&app, file, &content)?;
        Ok(true)
    })
    .await
    .map_err(|_| "Could not complete tariff export".to_owned())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_all_cells_and_season_metadata() {
        let mut rates = vec![vec![0.31; 7]; 48];
        rates[0][0] = 0.1234;
        let plan = serde_json::json!({
            "version": 2, "name": "Isolated draft", "currency": "USD",
            "timeZone": "America/Los_Angeles", "billingDay": 17,
            "rates": rates, "seasons": [{"name": "Summer", "months": [6,7,8,9], "rates": rates}]
        });
        let decoded: Value = serde_json::from_str(&serialize(&plan).unwrap()).unwrap();
        assert_eq!(decoded, plan);
    }

    #[test]
    fn exports_all_twelve_seasons_even_when_pretty_json_exceeds_import_limit() {
        let rates = vec![vec![0.123456789012345; 7]; 48];
        let seasons: Vec<Value> = (1..=12)
            .map(|month| {
                serde_json::json!({
                    "name": format!("Month {month}"), "months": [month], "rates": rates
                })
            })
            .collect();
        let plan = serde_json::json!({
            "version": 2, "name": "All months", "currency": "USD",
            "timeZone": "America/Los_Angeles", "source": "manual",
            "rates": rates, "seasons": seasons
        });
        assert!(plan.to_string().len() < 100_000);
        let encoded = serialize(&plan).unwrap();
        assert!(encoded.len() > 100_000);
        assert_eq!(serde_json::from_str::<Value>(&encoded).unwrap(), plan);
    }

    #[test]
    fn rejects_invalid_or_oversized_documents_before_opening_dialog() {
        for plan in [
            Value::Null,
            serde_json::json!([]),
            serde_json::json!({"version": 1}),
        ] {
            assert!(serialize(&plan).is_err());
        }
        let plan = serde_json::json!({"version": 2, "name": "x".repeat(1_048_577)});
        assert_eq!(serialize(&plan).unwrap_err(), "Tariff export is too large");
    }
}
