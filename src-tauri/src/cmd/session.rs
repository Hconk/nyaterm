use crate::app_core::NyatermCore;
use crate::config;
use crate::core::ssh::{
    self, HostKeyVerifyManager, PendingAuthManager, PendingSshAuthManager, SshAuthResponse,
};
use crate::core::{
    self, QuickCommandsStore, RecordingManager, SessionCommand, SessionInfo, SessionManager,
    TerminalHistorySearchRequest, TerminalHistorySearchResponse,
};
use crate::error::{AppError, AppResult};
use crate::observability::{self, StructuredLog, StructuredLogLevel};
use crate::utils::fuzzy::{
    FuzzyCandidateResult, FuzzyResult, FuzzySearchCandidate,
    fuzzy_search_candidates as fuzzy_search_candidate_items, fuzzy_search_items,
};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{Emitter, Manager};

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartupCommandPayload {
    command: String,
    delay_ms: u64,
}

#[tauri::command]
pub async fn create_ssh_session(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, Arc<SessionManager>>,
    recording_state: tauri::State<'_, Arc<RecordingManager>>,
    connection_id: String,
    create_request_id: Option<String>,
    startup_command: Option<StartupCommandPayload>,
) -> AppResult<String> {
    let ssh_config = ssh::load_saved_ssh_config(&app, &connection_id)?;
    let pending_creation = state.begin_session_creation(create_request_id).await;
    let (guard, cancel_rx) = match pending_creation {
        Some((guard, cancel_rx)) => (Some(guard), Some(cancel_rx)),
        None => (None, None),
    };

    let session_id = ssh::create_ssh_session(
        app.clone(),
        state.inner().clone(),
        ssh_config,
        Some(connection_id.clone()),
        Some(window.label().to_string()),
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
    maybe_start_auto_recording(
        &app,
        state.inner().as_ref(),
        recording_state.inner().clone(),
        &session_id,
    )
    .await;
    Ok(session_id)
}

#[tauri::command]
pub async fn create_multiplexed_ssh_session(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<SessionManager>>,
    recording_state: tauri::State<'_, Arc<RecordingManager>>,
    source_session_id: String,
    startup_command: Option<StartupCommandPayload>,
) -> AppResult<String> {
    let session_id = ssh::create_multiplexed_ssh_session(
        app.clone(),
        state.inner().clone(),
        &source_session_id,
        startup_command.map(|command| ssh::SshStartupCommand {
            command: command.command,
            delay_ms: command.delay_ms,
        }),
    )
    .await?;
    maybe_start_auto_recording(
        &app,
        state.inner().as_ref(),
        recording_state.inner().clone(),
        &session_id,
    )
    .await;
    Ok(session_id)
}

#[tauri::command]
pub async fn create_local_session(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, Arc<SessionManager>>,
    recording_state: tauri::State<'_, Arc<RecordingManager>>,
    connection_id: Option<String>,
    create_request_id: Option<String>,
) -> AppResult<String> {
    let pending_creation = state.begin_session_creation(create_request_id).await;
    let (guard, _cancel_rx) = match pending_creation {
        Some((guard, cancel_rx)) => (Some(guard), Some(cancel_rx)),
        None => (None, None),
    };
    let config = if let Some(ref cid) = connection_id {
        let conn = config::load_connection_by_id(&app, cid)?;
        match conn.config {
            config::ConnectionType::LocalTerminal {
                shell_path,
                shell_args,
                working_dir,
                ..
            } => Some(core::LocalSessionConfig {
                shell_path,
                shell_args,
                working_dir,
                name: conn.name,
            }),
            _ => None,
        }
    } else {
        None
    };
    let session_id = core::create_local_session(
        app.clone(),
        state.inner().clone(),
        config,
        Some(window.label().to_string()),
    )
    .await?;
    drop(guard);
    if let Some(connection_id) = connection_id {
        if let Err(error) = crate::storage::mark_connection_used(&connection_id) {
            tracing::warn!(connection_id, %error, "Failed to mark connection as recently used");
        }
    }
    maybe_start_auto_recording(
        &app,
        state.inner().as_ref(),
        recording_state.inner().clone(),
        &session_id,
    )
    .await;
    Ok(session_id)
}

#[tauri::command]
pub async fn create_telnet_session(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, Arc<SessionManager>>,
    recording_state: tauri::State<'_, Arc<RecordingManager>>,
    connection_id: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    name: Option<String>,
    create_request_id: Option<String>,
) -> AppResult<String> {
    let pending_creation = state.begin_session_creation(create_request_id).await;
    let (guard, _cancel_rx) = match pending_creation {
        Some((guard, cancel_rx)) => (Some(guard), Some(cancel_rx)),
        None => (None, None),
    };
    let cfg = if let Some(ref cid) = connection_id {
        let conn = config::load_connection_by_id(&app, cid)?;
        match conn.config {
            config::ConnectionType::Telnet {
                host: ref ch,
                port: cp,
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
                host: ch.clone(),
                port: cp,
                name: conn.name.clone(),
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
        state.inner().clone(),
        cfg,
        connection_id,
        Some(window.label().to_string()),
    )
    .await?;
    drop(guard);
    if let Some(connection_id) = marked_connection_id {
        if let Err(error) = crate::storage::mark_connection_used(&connection_id) {
            tracing::warn!(connection_id, %error, "Failed to mark connection as recently used");
        }
    }
    maybe_start_auto_recording(
        &app,
        state.inner().as_ref(),
        recording_state.inner().clone(),
        &session_id,
    )
    .await;
    Ok(session_id)
}

#[tauri::command]
pub async fn create_serial_session(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, Arc<SessionManager>>,
    recording_state: tauri::State<'_, Arc<RecordingManager>>,
    connection_id: Option<String>,
    port_name: Option<String>,
    baud_rate: Option<u32>,
    data_bits: Option<u8>,
    parity: Option<String>,
    stop_bits: Option<String>,
    name: Option<String>,
    create_request_id: Option<String>,
) -> AppResult<String> {
    let pending_creation = state.begin_session_creation(create_request_id).await;
    let (guard, _cancel_rx) = match pending_creation {
        Some((guard, cancel_rx)) => (Some(guard), Some(cancel_rx)),
        None => (None, None),
    };
    let cfg = if let Some(ref cid) = connection_id {
        let conn = config::load_connection_by_id(&app, cid)?;
        match conn.config {
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
                name: conn.name,
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
        state.inner().clone(),
        cfg,
        connection_id,
        Some(window.label().to_string()),
    )
    .await?;
    drop(guard);
    if let Some(connection_id) = marked_connection_id {
        if let Err(error) = crate::storage::mark_connection_used(&connection_id) {
            tracing::warn!(connection_id, %error, "Failed to mark connection as recently used");
        }
    }
    maybe_start_auto_recording(
        &app,
        state.inner().as_ref(),
        recording_state.inner().clone(),
        &session_id,
    )
    .await;
    Ok(session_id)
}

async fn maybe_start_auto_recording(
    app: &tauri::AppHandle,
    session_manager: &SessionManager,
    recording_manager: Arc<RecordingManager>,
    session_id: &str,
) {
    let settings = match config::load_app_settings(app) {
        Ok(settings) => settings,
        Err(error) => {
            tracing::warn!(session_id, %error, "Failed to load settings for auto recording");
            return;
        }
    };
    let transfer = settings.transfer;
    if !transfer.recording_auto_start {
        return;
    }

    let session_info = match session_manager.session_info(session_id).await {
        Ok(info) => info,
        Err(error) => {
            tracing::warn!(session_id, %error, "Failed to load session info for auto recording");
            return;
        }
    };

    let file_path =
        match build_auto_recording_file_path(app, &transfer.recording_path, &session_info.name) {
            Ok(path) => path,
            Err(error) => {
                tracing::warn!(session_id, %error, "Failed to build auto recording path");
                return;
            }
        };
    let file_path = file_path.to_string_lossy().to_string();
    let include_io_labels = transfer.recording_include_io_labels;
    let include_timestamps = transfer.recording_include_timestamps;
    let task_session_id = session_id.to_string();
    let log_session_id = task_session_id.clone();

    let result = tokio::task::spawn_blocking(move || {
        recording_manager.start(
            &task_session_id,
            &file_path,
            include_io_labels,
            include_timestamps,
        )
    })
    .await;

    match result {
        Ok(Ok(())) => {
            let _ = app.emit("sessions-changed", ());
        }
        Ok(Err(error)) => {
            tracing::warn!(session_id = %log_session_id, %error, "Failed to auto-start recording");
        }
        Err(error) => {
            tracing::warn!(session_id = %log_session_id, %error, "Auto recording task failed");
        }
    }
}

fn build_auto_recording_file_path(
    app: &tauri::AppHandle,
    configured_dir: &str,
    session_name: &str,
) -> AppResult<PathBuf> {
    let dir = if configured_dir.trim().is_empty() {
        default_recording_dir(app)?
    } else {
        PathBuf::from(configured_dir)
    };
    let timestamp = time::OffsetDateTime::now_local()
        .unwrap_or_else(|_| time::OffsetDateTime::now_utc())
        .format(time::macros::format_description!(
            "[year]-[month]-[day]T[hour]-[minute]-[second]"
        ))
        .unwrap_or_else(|_| "1970-01-01T00-00-00".to_string());

    Ok(dir.join(format!(
        "recording-{}-{timestamp}.log",
        safe_recording_name(session_name)
    )))
}

fn default_recording_dir(app: &tauri::AppHandle) -> AppResult<PathBuf> {
    match app.path().download_dir() {
        Ok(path) => Ok(path),
        Err(_) => dirs::download_dir()
            .or_else(|| dirs::home_dir().map(|home| home.join("Downloads")))
            .ok_or_else(|| AppError::Config("Failed to resolve Downloads directory".to_string())),
    }
}

fn safe_recording_name(name: &str) -> String {
    let mut safe = String::new();
    let mut last_was_replacement = false;

    for ch in name.chars() {
        if ch.is_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            safe.push(ch);
            last_was_replacement = false;
        } else if !last_was_replacement {
            safe.push('_');
            last_was_replacement = true;
        }
    }

    if safe.is_empty() {
        "session".to_string()
    } else {
        safe
    }
}

#[cfg(test)]
mod tests {
    use super::safe_recording_name;

    #[test]
    fn safe_recording_name_preserves_readable_parts() {
        assert_eq!(safe_recording_name(""), "session");
        assert_eq!(safe_recording_name("my session!/prod"), "my_session_prod");
        assert_eq!(safe_recording_name("中台 算法库"), "中台_算法库");
        assert_eq!(safe_recording_name("ssh.host_01-prod"), "ssh.host_01-prod");
    }
}

#[tauri::command]
pub async fn cancel_session_creation(
    state: tauri::State<'_, Arc<SessionManager>>,
    create_request_id: String,
) -> AppResult<bool> {
    Ok(state.cancel_session_creation(&create_request_id).await)
}

#[tauri::command]
pub fn list_serial_ports() -> AppResult<Vec<String>> {
    core::list_serial_ports()
}

#[tauri::command]
pub async fn write_to_session(
    core: tauri::State<'_, NyatermCore>,
    session_id: String,
    data: String,
) -> AppResult<()> {
    core.write_to_session(&session_id, data).await
}

#[tauri::command]
pub async fn set_session_output_paused(
    core: tauri::State<'_, NyatermCore>,
    session_id: String,
    paused: bool,
) -> AppResult<()> {
    core.set_session_output_paused(&session_id, paused).await
}

#[tauri::command]
pub async fn zmodem_accept_download(
    core: tauri::State<'_, NyatermCore>,
    session_id: String,
    save_dir: String,
) -> AppResult<()> {
    core.zmodem_accept_download(&session_id, save_dir).await
}

#[tauri::command]
pub async fn zmodem_accept_upload(
    core: tauri::State<'_, NyatermCore>,
    session_id: String,
    file_paths: Vec<String>,
) -> AppResult<()> {
    core.zmodem_accept_upload(&session_id, file_paths).await
}

#[tauri::command]
pub async fn zmodem_cancel(
    core: tauri::State<'_, NyatermCore>,
    session_id: String,
) -> AppResult<()> {
    core.zmodem_cancel(&session_id).await
}

#[tauri::command]
pub async fn resize_session(
    core: tauri::State<'_, NyatermCore>,
    session_id: String,
    cols: u32,
    rows: u32,
) -> AppResult<()> {
    core.resize_session(&session_id, cols, rows).await
}

#[tauri::command]
pub async fn attach_session(
    core: tauri::State<'_, NyatermCore>,
    session_id: String,
) -> AppResult<()> {
    core.attach_session(&session_id).await
}

#[tauri::command]
pub async fn close_session(
    app: tauri::AppHandle,
    core: tauri::State<'_, NyatermCore>,
    session_id: String,
) -> AppResult<()> {
    core.close_session(app, session_id).await
}

#[tauri::command]
pub async fn list_sessions(core: tauri::State<'_, NyatermCore>) -> AppResult<Vec<SessionInfo>> {
    core.list_sessions().await
}

#[tauri::command]
pub async fn add_command_history(
    state: tauri::State<'_, Arc<SessionManager>>,
    session_id: String,
    command: String,
) -> AppResult<()> {
    state.add_command(&session_id, command).await;
    Ok(())
}

#[tauri::command]
pub async fn register_command_submission(
    state: tauri::State<'_, Arc<SessionManager>>,
    session_id: String,
    command: String,
) -> AppResult<()> {
    state
        .register_command_submission(&session_id, command)
        .await;
    Ok(())
}

#[tauri::command]
pub async fn get_command_history(
    state: tauri::State<'_, Arc<SessionManager>>,
) -> AppResult<Vec<String>> {
    Ok(state.get_all_history().await)
}

#[tauri::command]
pub async fn delete_command_history(
    state: tauri::State<'_, Arc<SessionManager>>,
    command: String,
) -> AppResult<()> {
    state.delete_history_command(command).await;
    Ok(())
}

#[tauri::command]
pub async fn fuzzy_search_history(
    state: tauri::State<'_, Arc<SessionManager>>,
    pattern: String,
    limit: usize,
    min_command_length: Option<usize>,
    max_command_length: Option<usize>,
) -> AppResult<Vec<FuzzyResult>> {
    Ok(state
        .fuzzy_search(&pattern, limit, min_command_length, max_command_length)
        .await)
}

#[tauri::command]
pub async fn fuzzy_search_commands(
    state: tauri::State<'_, Arc<QuickCommandsStore>>,
    pattern: String,
    limit: usize,
) -> AppResult<Vec<FuzzyResult>> {
    let cfg = state.snapshot();
    let items: Vec<(&str, &str)> = cfg
        .commands
        .iter()
        .map(|c| (c.label.as_str(), c.command.as_str()))
        .collect();
    Ok(fuzzy_search_items(
        &items,
        &pattern,
        "quickCommand",
        limit,
        None,
        None,
    ))
}

#[tauri::command]
pub async fn fuzzy_search_candidates(
    pattern: String,
    items: Vec<FuzzySearchCandidate>,
    limit: usize,
) -> AppResult<Vec<FuzzyCandidateResult>> {
    Ok(fuzzy_search_candidate_items(&items, &pattern, limit))
}

#[tauri::command]
pub async fn start_recording(
    state: tauri::State<'_, Arc<RecordingManager>>,
    session_id: String,
    file_path: String,
    include_io_labels: bool,
    include_timestamps: bool,
) -> AppResult<()> {
    let mgr = state.inner().clone();
    tokio::task::spawn_blocking(move || {
        mgr.start(
            &session_id,
            &file_path,
            include_io_labels,
            include_timestamps,
        )
    })
    .await
    .map_err(|e| AppError::Config(format!("Task join error: {e}")))?
}

#[tauri::command]
pub async fn stop_recording(
    state: tauri::State<'_, Arc<RecordingManager>>,
    session_id: String,
) -> AppResult<String> {
    let mgr = state.inner().clone();
    tokio::task::spawn_blocking(move || mgr.stop(&session_id))
        .await
        .map_err(|e| AppError::Config(format!("Task join error: {e}")))?
}

#[tauri::command]
pub async fn is_recording(
    state: tauri::State<'_, Arc<RecordingManager>>,
    session_id: String,
) -> AppResult<bool> {
    Ok(state.is_recording(&session_id))
}

#[tauri::command]
pub async fn save_session_transcript(
    state: tauri::State<'_, Arc<RecordingManager>>,
    session_id: String,
    file_path: String,
    include_io_labels: bool,
    include_timestamps: bool,
) -> AppResult<String> {
    let mgr = state.inner().clone();
    tokio::task::spawn_blocking(move || {
        mgr.save_transcript(
            &session_id,
            &file_path,
            include_io_labels,
            include_timestamps,
        )
    })
    .await
    .map_err(|e| AppError::Config(format!("Task join error: {e}")))?
}

#[tauri::command]
pub async fn terminal_history_search(
    state: tauri::State<'_, Arc<RecordingManager>>,
    request: TerminalHistorySearchRequest,
) -> AppResult<TerminalHistorySearchResponse> {
    let mgr = state.inner().clone();
    tokio::task::spawn_blocking(move || mgr.search_history(request))
        .await
        .map_err(|e| AppError::Config(format!("Task join error: {e}")))?
}

#[tauri::command]
pub async fn list_recording_sessions(
    state: tauri::State<'_, Arc<RecordingManager>>,
) -> AppResult<Vec<String>> {
    Ok(state.list_recording_sessions())
}

#[tauri::command]
pub async fn set_recording_memory_limit(
    state: tauri::State<'_, Arc<RecordingManager>>,
    max_bytes: usize,
) -> AppResult<()> {
    state.set_memory_limit(max_bytes);
    Ok(())
}

#[tauri::command]
pub async fn submit_otp_response(
    state: tauri::State<'_, Arc<PendingAuthManager>>,
    request_id: String,
    responses: Vec<String>,
) -> AppResult<()> {
    if state.respond(&request_id, Some(responses)).await {
        observability::log_event(StructuredLog {
            level: StructuredLogLevel::Info,
            domain: "security.flow".to_string(),
            event: "otp.response_received".to_string(),
            message: "Received OTP response from frontend".to_string(),
            ids: Some(serde_json::json!({ "request_id": request_id })),
            data: None,
            error: None,
            client_timestamp: None,
        });
        Ok(())
    } else {
        observability::log_event(StructuredLog {
            level: StructuredLogLevel::Warn,
            domain: "security.flow".to_string(),
            event: "otp.response_rejected".to_string(),
            message: "Rejected OTP response for missing request".to_string(),
            ids: Some(serde_json::json!({ "request_id": request_id.clone() })),
            data: None,
            error: None,
            client_timestamp: None,
        });
        Err(AppError::Auth(format!(
            "No pending OTP request with id '{}'",
            request_id
        )))
    }
}

#[tauri::command]
pub async fn cancel_otp_request(
    state: tauri::State<'_, Arc<PendingAuthManager>>,
    request_id: String,
) -> AppResult<()> {
    let cancelled = state.respond(&request_id, None).await;
    observability::log_event(StructuredLog {
        level: if cancelled {
            StructuredLogLevel::Info
        } else {
            StructuredLogLevel::Warn
        },
        domain: "security.flow".to_string(),
        event: if cancelled {
            "otp.request_cancelled".to_string()
        } else {
            "otp.request_cancel_missing".to_string()
        },
        message: if cancelled {
            "Cancelled OTP request".to_string()
        } else {
            "OTP request was already missing when cancellation arrived".to_string()
        },
        ids: Some(serde_json::json!({ "request_id": request_id })),
        data: None,
        error: None,
        client_timestamp: None,
    });
    Ok(())
}

#[tauri::command]
pub async fn submit_ssh_auth_response(
    state: tauri::State<'_, Arc<PendingSshAuthManager>>,
    request_id: String,
    response: SshAuthResponse,
) -> AppResult<()> {
    if state.respond(&request_id, Some(response)).await {
        observability::log_event(StructuredLog {
            level: StructuredLogLevel::Info,
            domain: "security.flow".to_string(),
            event: "ssh_auth.response_received".to_string(),
            message: "Received SSH credential response from frontend".to_string(),
            ids: Some(serde_json::json!({ "request_id": request_id })),
            data: None,
            error: None,
            client_timestamp: None,
        });
        Ok(())
    } else {
        Err(AppError::Auth(format!(
            "No pending SSH authentication request with id '{}'",
            request_id
        )))
    }
}

#[tauri::command]
pub async fn cancel_ssh_auth_request(
    state: tauri::State<'_, Arc<PendingSshAuthManager>>,
    request_id: String,
) -> AppResult<()> {
    let cancelled = state.respond(&request_id, None).await;
    observability::log_event(StructuredLog {
        level: if cancelled {
            StructuredLogLevel::Info
        } else {
            StructuredLogLevel::Warn
        },
        domain: "security.flow".to_string(),
        event: if cancelled {
            "ssh_auth.request_cancelled".to_string()
        } else {
            "ssh_auth.request_cancel_missing".to_string()
        },
        message: if cancelled {
            "Cancelled SSH credential request".to_string()
        } else {
            "SSH credential request was already missing when cancellation arrived".to_string()
        },
        ids: Some(serde_json::json!({ "request_id": request_id })),
        data: None,
        error: None,
        client_timestamp: None,
    });
    Ok(())
}

#[tauri::command]
pub async fn respond_host_key_verify(
    state: tauri::State<'_, Arc<HostKeyVerifyManager>>,
    request_id: String,
    accepted: bool,
) -> AppResult<()> {
    let resolved = state.respond(&request_id, accepted).await;
    observability::log_event(StructuredLog {
        level: if resolved {
            StructuredLogLevel::Info
        } else {
            StructuredLogLevel::Warn
        },
        domain: "security.flow".to_string(),
        event: if accepted {
            "host_key.accepted".to_string()
        } else {
            "host_key.rejected".to_string()
        },
        message: if resolved {
            format!(
                "Host key verification response received (accepted={})",
                accepted
            )
        } else {
            "Host key verification response for missing request".to_string()
        },
        ids: Some(serde_json::json!({ "request_id": request_id })),
        data: None,
        error: None,
        client_timestamp: None,
    });
    if resolved {
        Ok(())
    } else {
        Err(AppError::Auth(format!(
            "No pending host key verification with id '{}'",
            request_id
        )))
    }
}
