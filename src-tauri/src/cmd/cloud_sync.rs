use crate::app_core::NyatermCore;
use crate::config::{CloudSyncHistoryEntry, CloudSyncStatus};
use crate::core;
use crate::core::cloud_sync::{GithubGistDeviceFlowPoll, GithubGistDeviceFlowStart};
use crate::error::AppResult;

#[tauri::command]
pub async fn test_cloud_sync_connection(core: tauri::State<'_, NyatermCore>) -> AppResult<()> {
    core.test_cloud_sync_connection().await
}

#[tauri::command]
pub async fn get_cloud_sync_status(
    core: tauri::State<'_, NyatermCore>,
) -> AppResult<CloudSyncStatus> {
    core.get_cloud_sync_status().await
}

#[tauri::command]
pub async fn sync_push_now(core: tauri::State<'_, NyatermCore>) -> AppResult<()> {
    core.sync_push_now().await
}

#[tauri::command]
pub async fn sync_pull_now(core: tauri::State<'_, NyatermCore>) -> AppResult<()> {
    core.sync_pull_now().await
}

#[tauri::command]
pub async fn resolve_cloud_sync_conflict(
    core: tauri::State<'_, NyatermCore>,
    action: String,
) -> AppResult<()> {
    core.resolve_cloud_sync_conflict(&action).await
}

#[tauri::command]
pub async fn list_cloud_sync_history(
    core: tauri::State<'_, NyatermCore>,
) -> AppResult<Vec<CloudSyncHistoryEntry>> {
    core.list_cloud_sync_history().await
}

#[tauri::command]
pub async fn begin_github_gist_device_flow() -> AppResult<GithubGistDeviceFlowStart> {
    core::cloud_sync::begin_github_gist_device_flow().await
}

#[tauri::command]
pub async fn poll_github_gist_device_flow(
    flow_id: String,
    existing_gist_id: Option<String>,
) -> AppResult<GithubGistDeviceFlowPoll> {
    core::cloud_sync::poll_github_gist_device_flow(&flow_id, existing_gist_id).await
}

#[tauri::command]
pub async fn cancel_github_gist_device_flow(flow_id: String) -> AppResult<()> {
    core::cloud_sync::cancel_github_gist_device_flow(&flow_id).await;
    Ok(())
}
