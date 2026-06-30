//! Core application facade shared by Tauri and future native UI shells.
//!
//! This facade centralizes construction of long-lived backend managers.  It is
//! intentionally UI-toolkit agnostic so the existing Tauri WebView frontend and
//! a future egui frontend can share the same runtime services during migration.

use std::path::PathBuf;
use std::sync::Arc;

use crate::app_event::AppEventBus;
use crate::config::{
    CloudSyncHistoryEntry, CloudSyncStatus, QuickCommand, QuickCommandCategory, QuickCommandsConfig,
};
use crate::core::ai::AgentApprovalManager;
use crate::core::sftp::TransferDuplicateManager;
use crate::core::ssh::{
    HostKeyVerifyManager, PendingAuthManager, PendingSshAuthManager, TunnelManager,
};
use crate::core::{
    CloudSyncManager, QuickCommandsImportResult, QuickCommandsImportSource, QuickCommandsStore,
    RecordingManager, SessionCommand, SessionInfo, SessionManager,
};
use crate::error::{AppError, AppResult};
use crate::observability::{self, StructuredLog, StructuredLogLevel};
use crate::utils::fuzzy::FuzzyResult;
use tauri::Manager;

/// Shared backend managers and typed event publisher for UI frontends.
#[derive(Clone)]
pub struct NyatermCore {
    pub session_manager: Arc<SessionManager>,
    pub tunnel_manager: Arc<TunnelManager>,
    pub recording_manager: Arc<RecordingManager>,
    pub pending_auth_manager: Arc<PendingAuthManager>,
    pub pending_ssh_auth_manager: Arc<PendingSshAuthManager>,
    pub host_key_verify_manager: Arc<HostKeyVerifyManager>,
    pub quick_commands_store: Arc<QuickCommandsStore>,
    pub cloud_sync_manager: Arc<CloudSyncManager>,
    pub agent_approval_manager: Arc<AgentApprovalManager>,
    pub transfer_duplicate_manager: Arc<TransferDuplicateManager>,
    pub event_bus: AppEventBus,
}

impl NyatermCore {
    /// Build all backend managers used by the desktop application.
    pub fn new() -> Self {
        Self {
            session_manager: Arc::new(SessionManager::new()),
            tunnel_manager: Arc::new(TunnelManager::new()),
            recording_manager: Arc::new(RecordingManager::new()),
            pending_auth_manager: Arc::new(PendingAuthManager::new()),
            pending_ssh_auth_manager: Arc::new(PendingSshAuthManager::new()),
            host_key_verify_manager: Arc::new(HostKeyVerifyManager::new()),
            quick_commands_store: Arc::new(QuickCommandsStore::new()),
            cloud_sync_manager: Arc::new(CloudSyncManager::new()),
            agent_approval_manager: Arc::new(AgentApprovalManager::new()),
            transfer_duplicate_manager: Arc::new(TransferDuplicateManager::new()),
            event_bus: AppEventBus::new(),
        }
    }

    /// List active sessions for UI frontends.
    pub async fn list_sessions(&self) -> AppResult<Vec<SessionInfo>> {
        Ok(self.session_manager.list_sessions().await)
    }

    /// Write UTF-8 input bytes to a session.
    pub async fn write_to_session(&self, session_id: &str, data: String) -> AppResult<()> {
        self.session_manager
            .send_command(session_id, SessionCommand::Write(data.into_bytes()))
            .await
    }

    /// Pause or resume backend output forwarding for a session.
    pub async fn set_session_output_paused(&self, session_id: &str, paused: bool) -> AppResult<()> {
        let command = if paused {
            SessionCommand::PauseOutput
        } else {
            SessionCommand::ResumeOutput
        };
        self.session_manager.send_command(session_id, command).await
    }

    /// Resize a session terminal.
    pub async fn resize_session(&self, session_id: &str, cols: u32, rows: u32) -> AppResult<()> {
        self.session_manager
            .send_command(session_id, SessionCommand::Resize { cols, rows })
            .await
    }

    /// Attach a frontend listener to a session and flush any buffered output.
    pub async fn attach_session(&self, session_id: &str) -> AppResult<()> {
        self.session_manager
            .send_command(session_id, SessionCommand::Attach)
            .await
    }

    /// Accept a pending ZMODEM download for a session.
    pub async fn zmodem_accept_download(
        &self,
        session_id: &str,
        save_dir: String,
    ) -> AppResult<()> {
        self.session_manager
            .send_command(
                session_id,
                SessionCommand::ZmodemAcceptDownload {
                    save_dir: PathBuf::from(save_dir),
                },
            )
            .await
    }

    /// Accept a pending ZMODEM upload for a session.
    pub async fn zmodem_accept_upload(
        &self,
        session_id: &str,
        file_paths: Vec<String>,
    ) -> AppResult<()> {
        self.session_manager
            .send_command(
                session_id,
                SessionCommand::ZmodemAcceptUpload {
                    files: file_paths.into_iter().map(PathBuf::from).collect(),
                },
            )
            .await
    }

    /// Cancel a pending or active ZMODEM transfer for a session.
    pub async fn zmodem_cancel(&self, session_id: &str) -> AppResult<()> {
        self.session_manager
            .send_command(session_id, SessionCommand::ZmodemCancel)
            .await
    }

    /// Return the current quick-command configuration snapshot.
    pub fn get_quick_commands(&self) -> AppResult<QuickCommandsConfig> {
        Ok(self.quick_commands_store.snapshot())
    }

    /// Replace and persist the quick-command configuration.
    pub fn save_quick_commands(
        &self,
        app: &tauri::AppHandle,
        config: QuickCommandsConfig,
    ) -> AppResult<()> {
        self.quick_commands_store.save_all(app, config)
    }

    /// Insert or update a quick command and optionally add its category.
    pub fn upsert_quick_command(
        &self,
        app: &tauri::AppHandle,
        command: QuickCommand,
        new_category: Option<QuickCommandCategory>,
    ) -> AppResult<QuickCommandsConfig> {
        self.quick_commands_store.upsert(app, command, new_category)
    }

    /// Increment a quick command usage counter.
    pub fn increment_quick_command_use_count(
        &self,
        app: &tauri::AppHandle,
        id: &str,
    ) -> AppResult<()> {
        self.quick_commands_store.increment_use_count(app, id)
    }

    /// Import quick commands from a supported external file.
    pub fn import_quick_commands(
        &self,
        app: &tauri::AppHandle,
        file_path: &str,
        source: QuickCommandsImportSource,
    ) -> AppResult<QuickCommandsImportResult> {
        self.quick_commands_store
            .import_from_file(app, file_path, source)
    }

    /// Search quick commands using the shared fuzzy matching behavior.
    pub fn fuzzy_search_quick_commands(
        &self,
        pattern: &str,
        limit: usize,
    ) -> AppResult<Vec<FuzzyResult>> {
        let cfg = self.quick_commands_store.snapshot();
        let items: Vec<(&str, &str)> = cfg
            .commands
            .iter()
            .map(|command| (command.label.as_str(), command.command.as_str()))
            .collect();
        Ok(crate::utils::fuzzy::fuzzy_search_items(
            &items,
            pattern,
            "quickCommand",
            limit,
            None,
            None,
        ))
    }

    /// Add a command to the persisted command history.
    pub async fn add_command_history(&self, session_id: &str, command: String) -> AppResult<()> {
        self.session_manager.add_command(session_id, command).await;
        Ok(())
    }

    /// Register a submitted terminal command for history and suggestions.
    pub async fn register_command_submission(
        &self,
        session_id: &str,
        command: String,
    ) -> AppResult<()> {
        self.session_manager
            .register_command_submission(session_id, command)
            .await;
        Ok(())
    }

    /// Return all command history entries.
    pub async fn get_command_history(&self) -> AppResult<Vec<String>> {
        Ok(self.session_manager.get_all_history().await)
    }

    /// Delete a command history entry.
    pub async fn delete_command_history(&self, command: String) -> AppResult<()> {
        self.session_manager.delete_history_command(command).await;
        Ok(())
    }

    /// Search command history using the shared fuzzy matching behavior.
    pub async fn fuzzy_search_history(
        &self,
        pattern: &str,
        limit: usize,
        min_command_length: Option<usize>,
        max_command_length: Option<usize>,
    ) -> AppResult<Vec<FuzzyResult>> {
        Ok(self
            .session_manager
            .fuzzy_search(pattern, limit, min_command_length, max_command_length)
            .await)
    }

    /// Verify the configured cloud-sync backend is reachable.
    pub async fn test_cloud_sync_connection(&self) -> AppResult<()> {
        self.cloud_sync_manager.test_connection().await
    }

    /// Return the latest cloud-sync status snapshot.
    pub async fn get_cloud_sync_status(&self) -> AppResult<CloudSyncStatus> {
        Ok(self.cloud_sync_manager.get_status().await)
    }

    /// Push local configuration to the configured cloud-sync backend.
    pub async fn sync_push_now(&self) -> AppResult<()> {
        self.cloud_sync_manager.sync_push_now("manual_push").await
    }

    /// Pull remote configuration from the configured cloud-sync backend.
    pub async fn sync_pull_now(&self) -> AppResult<()> {
        self.cloud_sync_manager.sync_pull_now("manual_pull").await
    }

    /// Resolve the currently pending cloud-sync conflict.
    pub async fn resolve_cloud_sync_conflict(&self, action: &str) -> AppResult<()> {
        self.cloud_sync_manager
            .resolve_cloud_sync_conflict(action)
            .await
    }

    /// List persisted cloud-sync history entries.
    pub async fn list_cloud_sync_history(&self) -> AppResult<Vec<CloudSyncHistoryEntry>> {
        Ok(self.cloud_sync_manager.list_history().await)
    }

    /// Request a session close and clean up temporary files associated with it.
    pub async fn close_session(&self, app: tauri::AppHandle, session_id: String) -> AppResult<()> {
        let session_id_clone = session_id.clone();

        observability::log_event(StructuredLog {
            level: StructuredLogLevel::Info,
            domain: "session.lifecycle".to_string(),
            event: "session.close_requested".to_string(),
            message: "Closing session".to_string(),
            ids: Some(serde_json::json!({ "session_id": session_id.clone() })),
            data: None,
            error: None,
            client_timestamp: None,
        });

        let result = match self
            .session_manager
            .send_command(&session_id, SessionCommand::Close)
            .await
        {
            Err(AppError::SessionNotFound(_)) => Ok(()),
            other => other,
        };

        tauri::async_runtime::spawn(async move {
            if let Ok(temp_dir) = app.path().temp_dir() {
                let session_temp_dir = temp_dir.join("nyaterm").join(&session_id_clone);
                if session_temp_dir.exists() {
                    if let Err(error) = tokio::fs::remove_dir_all(&session_temp_dir).await {
                        observability::log_event(StructuredLog {
                            level: StructuredLogLevel::Warn,
                            domain: "session.lifecycle".to_string(),
                            event: "session.temp_cleanup_failed".to_string(),
                            message: "Failed to clean up session temp directory".to_string(),
                            ids: Some(serde_json::json!({ "session_id": session_id_clone })),
                            data: Some(serde_json::json!({
                                "temp_dir": session_temp_dir,
                            })),
                            error: Some(serde_json::json!({ "message": error.to_string() })),
                            client_timestamp: None,
                        });
                    } else {
                        observability::log_event(StructuredLog {
                            level: StructuredLogLevel::Info,
                            domain: "session.lifecycle".to_string(),
                            event: "session.temp_cleanup_succeeded".to_string(),
                            message: "Cleaned up session temp directory".to_string(),
                            ids: Some(serde_json::json!({ "session_id": session_id_clone })),
                            data: Some(serde_json::json!({
                                "temp_dir": session_temp_dir,
                            })),
                            error: None,
                            client_timestamp: None,
                        });
                    }
                }
            }
        });

        result
    }
}

impl Default for NyatermCore {
    fn default() -> Self {
        Self::new()
    }
}
