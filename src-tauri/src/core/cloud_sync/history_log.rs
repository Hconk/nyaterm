use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::config::{self, CloudSyncHistoryEntry};
use crate::error::AppResult;
use crate::observability::{
    self, LOG_FILE_PREFIX, LOG_FILE_SUFFIX, StructuredLog, StructuredLogLevel,
};
use serde_json::Value;

const HISTORY_LIMIT: usize = 100;
pub(super) const HISTORY_LOG_DOMAIN: &str = "cloud_sync.history";
pub(super) const HISTORY_LOG_EVENT: &str = "entry";

pub(super) fn log_history_entry(entry: &CloudSyncHistoryEntry) {
    observability::log_event(StructuredLog {
        level: history_log_level(entry.status.as_str()),
        domain: HISTORY_LOG_DOMAIN.to_string(),
        event: HISTORY_LOG_EVENT.to_string(),
        message: entry.message.clone(),
        ids: Some(serde_json::json!({ "history_id": entry.id })),
        data: Some(serde_json::json!({
            "id": entry.id,
            "timestamp_ms": entry.timestamp_ms,
            "kind": entry.kind,
            "status": entry.status,
            "trigger": entry.trigger,
            "provider": entry.provider,
            "revision": entry.revision,
            "duration_ms": entry.duration_ms,
        })),
        error: None,
        client_timestamp: None,
    });
}

fn history_log_level(status: &str) -> StructuredLogLevel {
    match status {
        "failed" => StructuredLogLevel::Error,
        "conflict" => StructuredLogLevel::Warn,
        _ => StructuredLogLevel::Info,
    }
}

pub(super) fn read_cloud_sync_history_from_logs(
    app: &tauri::AppHandle,
) -> AppResult<Vec<CloudSyncHistoryEntry>> {
    let retention_days = config::load_app_settings(app)
        .map(|settings| settings.diagnostics.retention_days)
        .unwrap_or(7);
    let log_dir = crate::runtime::log_dir(app)?;
    read_cloud_sync_history_from_log_dir(&log_dir, retention_days, HISTORY_LIMIT)
}

fn read_cloud_sync_history_from_log_dir(
    log_dir: &Path,
    retention_days: u32,
    limit: usize,
) -> AppResult<Vec<CloudSyncHistoryEntry>> {
    if limit == 0 {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();

    for path in collect_cloud_sync_log_files(log_dir, retention_days)? {
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(_) => continue,
        };

        for line in content.lines().rev() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(entry) = parse_history_entry(value) {
                entries.push(entry);
                if entries.len() >= limit {
                    entries.sort_by(|a, b| b.timestamp_ms.cmp(&a.timestamp_ms));
                    return Ok(entries);
                }
            }
        }
    }

    entries.sort_by(|a, b| b.timestamp_ms.cmp(&a.timestamp_ms));
    Ok(entries)
}

fn collect_cloud_sync_log_files(log_dir: &Path, retention_days: u32) -> AppResult<Vec<PathBuf>> {
    let min_modified = SystemTime::now()
        .checked_sub(Duration::from_secs(
            u64::from(retention_days.max(1)) * 24 * 60 * 60,
        ))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut files = Vec::new();

    for entry in std::fs::read_dir(log_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !is_cloud_sync_log_file(&path) {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if modified < min_modified {
            continue;
        }
        files.push((path, modified));
    }

    files.sort_by(|(left_path, left_modified), (right_path, right_modified)| {
        right_modified
            .cmp(left_modified)
            .then_with(|| left_path.cmp(right_path))
    });
    Ok(files.into_iter().map(|(path, _)| path).collect())
}

fn is_cloud_sync_log_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.starts_with(LOG_FILE_PREFIX) && value.ends_with(LOG_FILE_SUFFIX))
}

fn parse_history_entry(value: Value) -> Option<CloudSyncHistoryEntry> {
    let root = value.as_object()?;
    if root.get("domain")?.as_str()? != HISTORY_LOG_DOMAIN {
        return None;
    }
    if root.get("event")?.as_str()? != HISTORY_LOG_EVENT {
        return None;
    }

    let data = root.get("data")?.as_object()?;
    Some(CloudSyncHistoryEntry {
        id: data.get("id")?.as_str()?.to_string(),
        timestamp_ms: data.get("timestamp_ms")?.as_u64()?,
        kind: data.get("kind")?.as_str()?.to_string(),
        status: data.get("status")?.as_str()?.to_string(),
        trigger: data.get("trigger")?.as_str()?.to_string(),
        provider: data
            .get("provider")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        revision: data
            .get("revision")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        duration_ms: data.get("duration_ms").and_then(Value::as_u64),
        message: root.get("message")?.as_str()?.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_only_the_most_recent_history_entries() {
        let dir = temp_log_dir("recent-limit");
        write_history_log(
            &dir.join(format!("{LOG_FILE_PREFIX}-old.{LOG_FILE_SUFFIX}")),
            &[history_line("old-1", 1), history_line("old-2", 2)],
        );
        write_history_log(
            &dir.join(format!("{LOG_FILE_PREFIX}-new.{LOG_FILE_SUFFIX}")),
            &[
                history_line("new-3", 3),
                history_line("new-4", 4),
                history_line("new-5", 5),
            ],
        );

        let entries = read_cloud_sync_history_from_log_dir(&dir, 7, 2).expect("read history");

        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            vec!["new-5", "new-4"]
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reads_older_files_only_until_limit_is_filled() {
        let dir = temp_log_dir("cross-file-limit");
        write_history_log(
            &dir.join(format!("{LOG_FILE_PREFIX}-old.{LOG_FILE_SUFFIX}")),
            &[history_line("old-1", 1), history_line("old-2", 2)],
        );
        write_history_log(
            &dir.join(format!("{LOG_FILE_PREFIX}-new.{LOG_FILE_SUFFIX}")),
            &[history_line("new-3", 3)],
        );

        let entries = read_cloud_sync_history_from_log_dir(&dir, 7, 2).expect("read history");

        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            vec!["new-3", "old-2"]
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    fn temp_log_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nyaterm-cloud-sync-history-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("create temp log dir");
        dir
    }

    fn write_history_log(path: &Path, lines: &[String]) {
        std::fs::write(path, lines.join("\n")).expect("write history log");
    }

    fn history_line(id: &str, timestamp_ms: u64) -> String {
        serde_json::json!({
            "domain": HISTORY_LOG_DOMAIN,
            "event": HISTORY_LOG_EVENT,
            "message": format!("history {id}"),
            "data": {
                "id": id,
                "timestamp_ms": timestamp_ms,
                "kind": "sync",
                "status": "success",
                "trigger": "manual_push",
                "provider": "webdav",
                "revision": null,
                "duration_ms": 1,
            }
        })
        .to_string()
    }
}
