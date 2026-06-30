use crate::config;
use crate::core::CloudSyncManager;
use crate::error::{AppError, AppResult};
use crate::observability::{self, StructuredLog, StructuredLogLevel};
use crate::utils::crypto;
use crate::utils::fonts::{FontInfo, list_system_font_families, list_system_font_infos};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tauri::Emitter;

#[derive(Debug, Clone, Serialize)]
pub struct KeywordHighlightImportResult {
    pub imported_rules: usize,
    pub updated_rules: usize,
    pub total_rules: usize,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum KeywordHighlightImportFile {
    Config {
        keyword_highlights: Vec<config::KeywordHighlightRule>,
    },
    Rules(Vec<config::KeywordHighlightRule>),
}

fn schedule_cloud_sync_notify(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        crate::core::cloud_sync::notify_config_changed(&app).await;
    });
}

#[tauri::command]
pub async fn get_system_fonts() -> Vec<String> {
    tauri::async_runtime::spawn_blocking(list_system_font_families)
        .await
        .unwrap_or_default()
}

#[tauri::command]
pub async fn get_system_font_infos() -> Vec<FontInfo> {
    tauri::async_runtime::spawn_blocking(list_system_font_infos)
        .await
        .unwrap_or_default()
}

#[tauri::command]
pub fn get_app_settings(app: tauri::AppHandle) -> AppResult<config::AppSettings> {
    let mut settings = config::load_app_settings(&app)?;
    if settings.security.master_password.is_some() {
        settings.security.master_password = Some("__SET__".to_string());
    }
    settings.cloud_sync = config::mask_cloud_sync_settings(settings.cloud_sync);
    settings.ai = config::mask_ai_settings(settings.ai);
    Ok(settings)
}

#[tauri::command]
pub async fn save_app_settings(
    app: tauri::AppHandle,
    manager: tauri::State<'_, Arc<CloudSyncManager>>,
    settings: config::AppSettings,
) -> AppResult<()> {
    persist_app_settings(&app, manager.inner(), settings).await
}

#[tauri::command]
pub async fn import_keyword_highlight_rules(
    app: tauri::AppHandle,
    manager: tauri::State<'_, Arc<CloudSyncManager>>,
    file_path: String,
) -> AppResult<KeywordHighlightImportResult> {
    let raw = std::fs::read_to_string(file_path)
        .map_err(|error| AppError::Config(format!("Failed to read import file: {error}")))?;
    let rules = parse_keyword_highlight_import(&raw)?;

    let mut settings = config::load_app_settings(&app)?;
    let result =
        merge_keyword_highlight_rules(&mut settings.terminal.keyword_highlights, rules, || {
            uuid::Uuid::new_v4().to_string()
        })?;

    persist_app_settings(&app, manager.inner(), settings).await?;
    Ok(result)
}

pub async fn persist_app_settings(
    app: &tauri::AppHandle,
    manager: &Arc<CloudSyncManager>,
    mut settings: config::AppSettings,
) -> AppResult<()> {
    settings.appearance.normalize_terminal_font_family();

    let existing = match config::load_app_settings(app) {
        Ok(existing) => existing,
        Err(error) => {
            observability::log_event(StructuredLog {
                level: StructuredLogLevel::Error,
                domain: "settings.persistence".to_string(),
                event: "settings.load_failed".to_string(),
                message: "Failed to load existing app settings before save".to_string(),
                ids: None,
                data: None,
                error: Some(serde_json::json!({ "message": error.to_string() })),
                client_timestamp: None,
            });
            return Err(error);
        }
    };

    match settings.security.master_password.as_deref() {
        Some("__SET__") => {
            settings.security.master_password = existing.security.master_password;
        }
        Some("") => {
            return Err(AppError::Config(
                "Master password cannot be empty when enabled".to_string(),
            ));
        }
        None => {
            if existing.security.master_password.is_some() {
                let old_plain = crypto::decrypt_settings_secret(
                    existing.security.master_password.as_deref().unwrap(),
                )?;
                crypto::rewrap_master_key(Some(&old_plain), None)?;
                crypto::set_master_password(None);
            }
            settings.security.master_password = None;
        }
        Some(plain) => {
            let old_plain = existing
                .security
                .master_password
                .as_deref()
                .and_then(|ct| crypto::decrypt_settings_secret(ct).ok());

            crypto::rewrap_master_key(old_plain.as_deref(), Some(plain))?;
            crypto::set_master_password(Some(plain.to_string()));

            settings.security.master_password = Some(crypto::encrypt_settings_secret(plain)?);
        }
    }
    let merged_cloud_sync =
        config::merge_masked_cloud_sync_settings(&existing.cloud_sync, settings.cloud_sync);
    settings.cloud_sync = merged_cloud_sync.clone();
    let merged_ai = config::merge_masked_ai_settings(&existing.ai, settings.ai);
    settings.ai = merged_ai.clone();

    let mut persisted_settings = settings.clone();
    persisted_settings.cloud_sync = config::encrypt_cloud_sync_settings(merged_cloud_sync.clone())?;
    persisted_settings.ai = config::encrypt_ai_settings(merged_ai)?;

    if let Err(error) = config::save_app_settings(app, &persisted_settings) {
        observability::log_event(StructuredLog {
            level: StructuredLogLevel::Error,
            domain: "settings.persistence".to_string(),
            event: "settings.save_failed".to_string(),
            message: "Failed to persist app settings".to_string(),
            ids: None,
            data: Some(serde_json::json!({
                "diagnostics_level": settings.diagnostics.level.as_str(),
                "diagnostics_retention_days": settings.diagnostics.retention_days,
            })),
            error: Some(serde_json::json!({ "message": error.to_string() })),
            client_timestamp: None,
        });
        return Err(error);
    }

    manager.replace_settings(merged_cloud_sync).await?;
    schedule_cloud_sync_notify(app.clone());
    let _ = app.emit("settings-changed", ());
    crate::tray::schedule_refresh(app);

    Ok(())
}

#[tauri::command]
pub fn save_app_ui_settings(ui: config::UiConfig) -> AppResult<()> {
    use crate::storage::{self, SettingsDocKey};
    storage::update_settings_doc(
        SettingsDocKey::AppSettings,
        |settings: &mut config::AppSettings| {
            settings.ui = ui;
            Ok(())
        },
    )
}

#[tauri::command]
pub fn verify_master_password(app: tauri::AppHandle, password: String) -> AppResult<bool> {
    let settings = config::load_app_settings(&app)?;
    match settings.security.master_password {
        Some(ref ct) => {
            let stored = crypto::decrypt_settings_secret(ct)?;
            Ok(stored == password)
        }
        None => Ok(true),
    }
}

fn parse_keyword_highlight_import(raw: &str) -> AppResult<Vec<config::KeywordHighlightRule>> {
    let import_file: KeywordHighlightImportFile = serde_json::from_str(raw)
        .map_err(|error| AppError::Config(format!("Invalid highlight rules JSON: {error}")))?;

    Ok(match import_file {
        KeywordHighlightImportFile::Config { keyword_highlights } => keyword_highlights,
        KeywordHighlightImportFile::Rules(rules) => rules,
    })
}

fn merge_keyword_highlight_rules(
    existing: &mut Vec<config::KeywordHighlightRule>,
    imported: Vec<config::KeywordHighlightRule>,
    mut next_id: impl FnMut() -> String,
) -> AppResult<KeywordHighlightImportResult> {
    let mut imported_rules = 0;
    let mut updated_rules = 0;
    let mut indexes = existing
        .iter()
        .enumerate()
        .filter_map(|(index, rule)| (!rule.id.trim().is_empty()).then(|| (rule.id.clone(), index)))
        .collect::<HashMap<_, _>>();

    for mut rule in imported {
        rule.name = rule.name.trim().to_string();
        rule.patterns = rule
            .patterns
            .into_iter()
            .map(|pattern| pattern.trim().to_string())
            .filter(|pattern| !pattern.is_empty())
            .collect();

        if rule.name.is_empty() || rule.patterns.is_empty() {
            continue;
        }

        rule.id = rule.id.trim().to_string();
        if rule.id.is_empty() {
            rule.id = next_id();
        }

        if let Some(index) = indexes.get(&rule.id).copied() {
            existing[index] = rule;
            updated_rules += 1;
        } else {
            let id = rule.id.clone();
            existing.push(rule);
            indexes.insert(id, existing.len() - 1);
            imported_rules += 1;
        }
    }

    if imported_rules == 0 && updated_rules == 0 {
        return Err(AppError::Config(
            "No valid highlight rules found in import file".to_string(),
        ));
    }

    Ok(KeywordHighlightImportResult {
        imported_rules,
        updated_rules,
        total_rules: existing.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn existing_rule() -> config::KeywordHighlightRule {
        config::KeywordHighlightRule {
            id: "deploy-errors".to_string(),
            name: "Deploy Errors".to_string(),
            patterns: vec!["fatal".to_string()],
            color_dark: "#ff7b72".to_string(),
            color_light: "#cf222e".to_string(),
            enabled: true,
        }
    }

    #[test]
    fn import_keyword_highlight_rules_parses_object_format() {
        let raw = r##"{
            "keyword_highlights": [
                {
                    "id": "deploy-errors",
                    "name": "Deploy Errors",
                    "patterns": ["deploy failed"],
                    "color_dark": "#ff7b72",
                    "color_light": "#cf222e",
                    "enabled": true
                }
            ]
        }"##;

        let rules = parse_keyword_highlight_import(raw).expect("parse object");

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "deploy-errors");
        assert_eq!(rules[0].patterns, vec!["deploy failed"]);
    }

    #[test]
    fn import_keyword_highlight_rules_parses_array_format() {
        let raw = r#"[
            {
                "name": "Warnings",
                "patterns": ["warn"]
            }
        ]"#;

        let rules = parse_keyword_highlight_import(raw).expect("parse array");

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].name, "Warnings");
        assert_eq!(rules[0].color_dark, "#79c0ff");
        assert_eq!(rules[0].color_light, "#0969da");
        assert!(rules[0].enabled);
    }

    #[test]
    fn import_keyword_highlight_rules_updates_existing_id() {
        let mut existing = vec![existing_rule()];
        let imported = parse_keyword_highlight_import(
            r##"[{
                "id": "deploy-errors",
                "name": "Deploy Failures",
                "patterns": ["rollback required"],
                "color_dark": "#ffa198",
                "color_light": "#a40e26",
                "enabled": false
            }]"##,
        )
        .expect("parse");

        let result =
            merge_keyword_highlight_rules(&mut existing, imported, || "generated-id".to_string())
                .expect("merge");

        assert_eq!(result.imported_rules, 0);
        assert_eq!(result.updated_rules, 1);
        assert_eq!(result.total_rules, 1);
        assert_eq!(existing[0].name, "Deploy Failures");
        assert_eq!(existing[0].patterns, vec!["rollback required"]);
        assert!(!existing[0].enabled);
    }

    #[test]
    fn import_keyword_highlight_rules_adds_generated_id_and_defaults() {
        let mut existing = Vec::new();
        let imported = parse_keyword_highlight_import(
            r#"[{
                "name": " Status ",
                "patterns": [" success ", "", " done "]
            }]"#,
        )
        .expect("parse");

        let result =
            merge_keyword_highlight_rules(&mut existing, imported, || "generated-id".to_string())
                .expect("merge");

        assert_eq!(result.imported_rules, 1);
        assert_eq!(result.updated_rules, 0);
        assert_eq!(existing[0].id, "generated-id");
        assert_eq!(existing[0].name, "Status");
        assert_eq!(existing[0].patterns, vec!["success", "done"]);
        assert_eq!(existing[0].color_dark, "#79c0ff");
        assert_eq!(existing[0].color_light, "#0969da");
        assert!(existing[0].enabled);
    }

    #[test]
    fn import_keyword_highlight_rules_rejects_empty_or_invalid_rules() {
        let mut existing = Vec::new();
        let imported = parse_keyword_highlight_import(
            r#"{
                "keyword_highlights": [
                    { "name": "", "patterns": ["fatal"] },
                    { "name": "Empty Patterns", "patterns": ["", " "] }
                ]
            }"#,
        )
        .expect("parse");

        let error =
            merge_keyword_highlight_rules(&mut existing, imported, || "generated-id".to_string())
                .expect_err("invalid rules should fail");

        assert!(error.to_string().contains("No valid highlight rules"));
        assert!(existing.is_empty());
    }
}
