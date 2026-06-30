//! Core application facade shared by Tauri and future native UI shells.
//!
//! This facade centralizes construction of long-lived backend managers.  It is
//! intentionally UI-toolkit agnostic so the existing Tauri WebView frontend and
//! a future egui frontend can share the same runtime services during migration.

use std::path::PathBuf;
use std::sync::Arc;

use crate::app_event::AppEventBus;
use crate::config::{
    self, AppSettings, CloudSyncHistoryEntry, CloudSyncStatus, Group, QuickCommand,
    QuickCommandCategory, QuickCommandsConfig, SavedConnection,
};
use crate::core::ai::AgentApprovalManager;
use crate::core::sftp::TransferDuplicateManager;
use crate::core::ssh::{
    self, HostKeyVerifyManager, PendingAuthManager, PendingSshAuthManager, TunnelManager,
};
use crate::core::{
    self, CloudSyncManager, QuickCommandsImportResult, QuickCommandsImportSource,
    QuickCommandsStore, RecordingManager, SessionCommand, SessionInfo, SessionManager,
};
use crate::error::{AppError, AppResult};
use crate::observability::{self, StructuredLog, StructuredLogLevel};
use crate::utils::crypto;
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

    /// Create an SSH session from a saved connection.
    pub async fn create_ssh_session(
        &self,
        app: tauri::AppHandle,
        connection_id: String,
        window_label: Option<String>,
        create_request_id: Option<String>,
        startup_command: Option<crate::cmd::session::StartupCommandPayload>,
    ) -> AppResult<String> {
        let ssh_config = ssh::load_saved_ssh_config(&app, &connection_id)?;
        let pending_creation = self
            .session_manager
            .begin_session_creation(create_request_id)
            .await;
        let (guard, cancel_rx) = match pending_creation {
            Some((guard, cancel_rx)) => (Some(guard), Some(cancel_rx)),
            None => (None, None),
        };

        let session_id = ssh::create_ssh_session(
            app.clone(),
            self.session_manager.clone(),
            ssh_config,
            Some(connection_id.clone()),
            window_label,
            cancel_rx,
            startup_command.map(|command| ssh::SshStartupCommand {
                command: command.command,
                delay_ms: command.delay_ms,
            }),
        )
        .await?;
        drop(guard);
        if let Err(error) = crate::storage::mark_connection_used(&connection_id) {
            tracing::warn!(connection_id, %error, "Failed to mark connection as recently used");
        }
        crate::cmd::session::maybe_start_auto_recording(
            &app,
            self.session_manager.as_ref(),
            self.recording_manager.clone(),
            &session_id,
        )
        .await;
        Ok(session_id)
    }

    /// Create a multiplexed SSH session from an existing SSH transport.
    pub async fn create_multiplexed_ssh_session(
        &self,
        app: tauri::AppHandle,
        source_session_id: &str,
        startup_command: Option<crate::cmd::session::StartupCommandPayload>,
    ) -> AppResult<String> {
        let session_id = ssh::create_multiplexed_ssh_session(
            app.clone(),
            self.session_manager.clone(),
            source_session_id,
            startup_command.map(|command| ssh::SshStartupCommand {
                command: command.command,
                delay_ms: command.delay_ms,
            }),
        )
        .await?;
        crate::cmd::session::maybe_start_auto_recording(
            &app,
            self.session_manager.as_ref(),
            self.recording_manager.clone(),
            &session_id,
        )
        .await;
        Ok(session_id)
    }

    /// Create a local PTY-backed session.
    pub async fn create_local_session(
        &self,
        app: tauri::AppHandle,
        connection_id: Option<String>,
        window_label: Option<String>,
        create_request_id: Option<String>,
    ) -> AppResult<String> {
        let pending_creation = self
            .session_manager
            .begin_session_creation(create_request_id)
            .await;
        let (guard, _cancel_rx) = match pending_creation {
            Some((guard, cancel_rx)) => (Some(guard), Some(cancel_rx)),
            None => (None, None),
        };
        let config = if let Some(ref connection_id) = connection_id {
            let connection = config::load_connection_by_id(&app, connection_id)?;
            match connection.config {
                config::ConnectionType::LocalTerminal {
                    shell_path,
                    shell_args,
                    working_dir,
                    ..
                } => Some(core::LocalSessionConfig {
                    shell_path,
                    shell_args,
                    working_dir,
                    name: connection.name,
                }),
                _ => None,
            }
        } else {
            None
        };
        let session_id = core::create_local_session(
            app.clone(),
            self.session_manager.clone(),
            config,
            window_label,
        )
        .await?;
        drop(guard);
        if let Some(connection_id) = connection_id {
            if let Err(error) = crate::storage::mark_connection_used(&connection_id) {
                tracing::warn!(connection_id, %error, "Failed to mark connection as recently used");
            }
        }
        crate::cmd::session::maybe_start_auto_recording(
            &app,
            self.session_manager.as_ref(),
            self.recording_manager.clone(),
            &session_id,
        )
        .await;
        Ok(session_id)
    }

    /// Create a Telnet session from either a saved connection or ad-hoc details.
    pub async fn create_telnet_session(
        &self,
        app: tauri::AppHandle,
        connection_id: Option<String>,
        host: Option<String>,
        port: Option<u16>,
        name: Option<String>,
        window_label: Option<String>,
        create_request_id: Option<String>,
    ) -> AppResult<String> {
        let pending_creation = self
            .session_manager
            .begin_session_creation(create_request_id)
            .await;
        let (guard, _cancel_rx) = match pending_creation {
            Some((guard, cancel_rx)) => (Some(guard), Some(cancel_rx)),
            None => (None, None),
        };
        let cfg = if let Some(ref connection_id) = connection_id {
            let connection = config::load_connection_by_id(&app, connection_id)?;
            match connection.config {
                config::ConnectionType::Telnet {
                    host: ref configured_host,
                    port: configured_port,
                    backspace_mode,
                    raw_tcp_cli,
                    enter_mode,
                    local_echo,
                    local_line_edit,
                    force_character_at_a_time,
                    send_naws,
                    send_sga,
                    ..
                } => core::TelnetSessionConfig {
                    host: configured_host.clone(),
                    port: configured_port,
                    name: connection.name.clone(),
                    backspace_mode,
                    raw_tcp_cli,
                    enter_mode: core::TelnetEnterMode::from_config_value(&enter_mode),
                    local_echo,
                    local_line_edit,
                    force_character_at_a_time,
                    send_naws,
                    send_sga,
                },
                _ => {
                    return Err(AppError::Config(
                        "Connection is not a Telnet connection".to_string(),
                    ));
                }
            }
        } else {
            core::TelnetSessionConfig {
                host: host.ok_or_else(|| AppError::Config("host is required".to_string()))?,
                port: port.unwrap_or(23),
                name: name.unwrap_or_else(|| "Telnet".to_string()),
                ..Default::default()
            }
        };
        let marked_connection_id = connection_id.clone();
        let session_id = core::create_telnet_session(
            app.clone(),
            self.session_manager.clone(),
            cfg,
            connection_id,
            window_label,
        )
        .await?;
        drop(guard);
        if let Some(connection_id) = marked_connection_id {
            if let Err(error) = crate::storage::mark_connection_used(&connection_id) {
                tracing::warn!(connection_id, %error, "Failed to mark connection as recently used");
            }
        }
        crate::cmd::session::maybe_start_auto_recording(
            &app,
            self.session_manager.as_ref(),
            self.recording_manager.clone(),
            &session_id,
        )
        .await;
        Ok(session_id)
    }

    /// Create a serial session from either a saved connection or ad-hoc port settings.
    pub async fn create_serial_session(
        &self,
        app: tauri::AppHandle,
        connection_id: Option<String>,
        port_name: Option<String>,
        baud_rate: Option<u32>,
        data_bits: Option<u8>,
        parity: Option<String>,
        stop_bits: Option<String>,
        name: Option<String>,
        window_label: Option<String>,
        create_request_id: Option<String>,
    ) -> AppResult<String> {
        let pending_creation = self
            .session_manager
            .begin_session_creation(create_request_id)
            .await;
        let (guard, _cancel_rx) = match pending_creation {
            Some((guard, cancel_rx)) => (Some(guard), Some(cancel_rx)),
            None => (None, None),
        };
        let cfg = if let Some(ref connection_id) = connection_id {
            let connection = config::load_connection_by_id(&app, connection_id)?;
            match connection.config {
                config::ConnectionType::Serial {
                    port_name,
                    baud_rate,
                    data_bits,
                    parity,
                    stop_bits,
                    backspace_mode,
                    ..
                } => core::SerialConfig {
                    port_name,
                    baud_rate,
                    data_bits,
                    parity,
                    stop_bits,
                    name: connection.name,
                    backspace_mode,
                },
                _ => {
                    return Err(AppError::Config(
                        "Connection is not a Serial connection".to_string(),
                    ));
                }
            }
        } else {
            core::SerialConfig {
                port_name: port_name
                    .ok_or_else(|| AppError::Config("port_name is required".to_string()))?,
                baud_rate: baud_rate.unwrap_or(115_200),
                data_bits: data_bits.unwrap_or(8),
                parity: parity.unwrap_or_else(|| "none".to_string()),
                stop_bits: stop_bits.unwrap_or_else(|| "1".to_string()),
                name: name.unwrap_or_else(|| "Serial".to_string()),
                backspace_mode: "ctrl_h".to_string(),
            }
        };
        let marked_connection_id = connection_id.clone();
        let session_id = core::create_serial_session(
            app.clone(),
            self.session_manager.clone(),
            cfg,
            connection_id,
            window_label,
        )
        .await?;
        drop(guard);
        if let Some(connection_id) = marked_connection_id {
            if let Err(error) = crate::storage::mark_connection_used(&connection_id) {
                tracing::warn!(connection_id, %error, "Failed to mark connection as recently used");
            }
        }
        crate::cmd::session::maybe_start_auto_recording(
            &app,
            self.session_manager.as_ref(),
            self.recording_manager.clone(),
            &session_id,
        )
        .await;
        Ok(session_id)
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

    /// Load saved connections with secrets stripped for UI consumption.
    pub fn get_saved_connections(&self, app: &tauri::AppHandle) -> AppResult<Vec<SavedConnection>> {
        let cfg = config::load_config(app)?;
        let mut connections = cfg.connections;
        for connection in &mut connections {
            if let Some(ref mut auth) = connection.auth {
                auth.has_password = auth.password.is_some();
                auth.password = None;
            }
        }
        Ok(connections)
    }

    /// Load saved connection groups.
    pub fn get_groups(&self, app: &tauri::AppHandle) -> AppResult<Vec<Group>> {
        let cfg = config::load_config(app)?;
        Ok(cfg.groups)
    }

    /// Insert or update a saved connection group.
    pub fn save_group(&self, app: &tauri::AppHandle, mut group: Group) -> AppResult<String> {
        let mut cfg = config::load_config(app)?;

        if group.id.is_empty() {
            group.id = uuid::Uuid::new_v4().to_string();
        }
        let target_id = group.id.clone();

        if let Some(existing) = cfg.groups.iter_mut().find(|item| item.id == target_id) {
            *existing = group;
        } else {
            cfg.groups.push(group);
        }

        config::save_config(app, &cfg)?;
        Ok(target_id)
    }

    /// Insert or update a saved connection while preserving encrypted secrets omitted by the UI.
    pub fn save_connection(
        &self,
        app: &tauri::AppHandle,
        connection: SavedConnection,
    ) -> AppResult<String> {
        crate::cmd::connection::save_connection_impl(app, connection)
    }

    /// Delete a saved connection by id.
    pub fn delete_connection(&self, app: &tauri::AppHandle, id: &str) -> AppResult<()> {
        let mut cfg = config::load_config(app)?;
        cfg.connections.retain(|connection| connection.id != id);
        config::save_config(app, &cfg)
    }

    /// Update persisted sort order for saved connections and groups.
    pub fn reorder_items(
        &self,
        app: &tauri::AppHandle,
        connections: &[crate::cmd::connection::SortOrderUpdate],
        groups: &[crate::cmd::connection::SortOrderUpdate],
    ) -> AppResult<()> {
        let mut cfg = config::load_config(app)?;
        for update in connections {
            if let Some(connection) = cfg
                .connections
                .iter_mut()
                .find(|connection| connection.id == update.id)
            {
                connection.sort_order = update.sort_order;
            }
        }
        for update in groups {
            if let Some(group) = cfg.groups.iter_mut().find(|group| group.id == update.id) {
                group.sort_order = update.sort_order;
            }
        }
        config::save_config(app, &cfg)
    }

    /// Delete a group, all descendant groups, and contained connections.
    pub fn delete_group(&self, app: &tauri::AppHandle, id: &str) -> AppResult<()> {
        let mut cfg = config::load_config(app)?;
        crate::cmd::connection::delete_group_from_config(&mut cfg, id);
        config::save_config(app, &cfg)
    }

    /// Remove all saved connections and groups.
    pub fn clear_all_connections(&self, app: &tauri::AppHandle) -> AppResult<()> {
        let mut cfg = config::load_config(app)?;
        cfg.connections.clear();
        cfg.groups.clear();
        config::save_config(app, &cfg)
    }

    /// Return the decrypted password stored directly on a saved connection.
    pub fn get_connection_password_value(
        &self,
        app: &tauri::AppHandle,
        id: &str,
    ) -> AppResult<Option<String>> {
        let connection = config::load_connection_by_id(app, id)?;
        let Some(auth) = connection.auth else {
            return Ok(None);
        };

        crypto::decrypt_optional(&auth.password)
    }

    /// Load app settings with sensitive values masked for UI editing.
    pub fn get_app_settings(&self, app: &tauri::AppHandle) -> AppResult<AppSettings> {
        let mut settings = config::load_app_settings(app)?;
        if settings.security.master_password.is_some() {
            settings.security.master_password = Some("__SET__".to_string());
        }
        settings.cloud_sync = config::mask_cloud_sync_settings(settings.cloud_sync);
        settings.ai = config::mask_ai_settings(settings.ai);
        Ok(settings)
    }

    /// Persist app settings through the shared settings persistence path.
    pub async fn save_app_settings(
        &self,
        app: &tauri::AppHandle,
        settings: AppSettings,
    ) -> AppResult<()> {
        crate::cmd::settings::persist_app_settings(app, &self.cloud_sync_manager, settings).await
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
