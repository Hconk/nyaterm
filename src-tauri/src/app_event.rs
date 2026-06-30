//! Typed application event bus for native UI frontends.
//!
//! The existing Tauri frontend consumes stringly-named window events.  Native
//! egui frontends need the same backend notifications without going through a
//! WebView, so this module provides a small typed bus that backend services can
//! adopt incrementally while the Tauri event bridge remains in place.

use serde::Serialize;
use tauri::{Emitter, Manager};
use tokio::sync::broadcast;

const DEFAULT_EVENT_BUFFER: usize = 1024;

/// High-level backend event consumed by native UI shells.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AppEvent {
    TerminalOutput { session_id: String, data: String },
    CwdChanged { session_id: String, cwd: String },
    SessionClosed { session_id: String },
    SessionsChanged,
    ConnectionsChanged,
    CommandHistoryChanged,
    Transfer { payload: serde_json::Value },
    OtpRequest { payload: serde_json::Value },
    SshAuthRequest { payload: serde_json::Value },
    HostKeyVerify { payload: serde_json::Value },
    CloudSyncStatusChanged { payload: serde_json::Value },
    CloudSyncHistoryChanged { payload: serde_json::Value },
    CloudSyncConflict { payload: serde_json::Value },
}

/// Cloneable publisher for typed app events.
#[derive(Debug, Clone)]
pub struct AppEventBus {
    sender: broadcast::Sender<AppEvent>,
}

impl AppEventBus {
    /// Create a bus with the default bounded broadcast buffer.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_EVENT_BUFFER)
    }

    /// Create a bus with an explicit bounded broadcast buffer.
    pub fn with_capacity(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity.max(1));
        Self { sender }
    }

    /// Subscribe to future app events.
    pub fn subscribe(&self) -> broadcast::Receiver<AppEvent> {
        self.sender.subscribe()
    }

    /// Publish an event. Returns the number of active receivers on success.
    pub fn publish(&self, event: AppEvent) -> Result<usize, broadcast::error::SendError<AppEvent>> {
        self.sender.send(event)
    }

    /// Publish an event and ignore the result when no native UI is attached yet.
    pub fn publish_lossy(&self, event: AppEvent) {
        let _ = self.publish(event);
    }
}

/// Publish a typed event through the bus managed by the Tauri app, if present.
///
/// This is intentionally lossy so existing Tauri-only startup paths keep working
/// while the native UI event bus is adopted incrementally.
pub fn publish_app_event(app: &tauri::AppHandle, event: AppEvent) {
    if let Some(bus) = app.try_state::<AppEventBus>() {
        bus.publish_lossy(event);
    }
}

fn emit_json_payload_event<T>(
    app: &tauri::AppHandle,
    event_name: &str,
    payload: &T,
    build_event: impl FnOnce(serde_json::Value) -> AppEvent,
) where
    T: Serialize,
{
    let _ = app.emit(event_name, payload);
    if let Ok(payload) = serde_json::to_value(payload) {
        publish_app_event(app, build_event(payload));
    }
}

/// Emit and publish an OTP / keyboard-interactive request.
pub fn emit_otp_request<T>(app: &tauri::AppHandle, payload: &T)
where
    T: Serialize,
{
    emit_json_payload_event(app, "otp-request", payload, |payload| {
        AppEvent::OtpRequest { payload }
    });
}

/// Emit and publish an SSH authentication request.
pub fn emit_ssh_auth_request<T>(app: &tauri::AppHandle, payload: &T)
where
    T: Serialize,
{
    emit_json_payload_event(app, "ssh-auth-request", payload, |payload| {
        AppEvent::SshAuthRequest { payload }
    });
}

/// Emit and publish an SSH host-key verification request.
pub fn emit_host_key_verify<T>(app: &tauri::AppHandle, payload: &T)
where
    T: Serialize,
{
    emit_json_payload_event(app, "host-key-verify", payload, |payload| {
        AppEvent::HostKeyVerify { payload }
    });
}

/// Emit and publish a cloud-sync status update.
pub fn emit_cloud_sync_status_changed<T>(app: &tauri::AppHandle, payload: &T)
where
    T: Serialize,
{
    emit_json_payload_event(app, "cloud-sync-status-changed", payload, |payload| {
        AppEvent::CloudSyncStatusChanged { payload }
    });
}

/// Emit and publish a cloud-sync history update.
pub fn emit_cloud_sync_history_changed<T>(app: &tauri::AppHandle, payload: &T)
where
    T: Serialize,
{
    emit_json_payload_event(app, "cloud-sync-history-changed", payload, |payload| {
        AppEvent::CloudSyncHistoryChanged { payload }
    });
}

/// Emit and publish a cloud-sync conflict update.
pub fn emit_cloud_sync_conflict<T>(app: &tauri::AppHandle, payload: &T)
where
    T: Serialize,
{
    emit_json_payload_event(app, "cloud-sync-conflict", payload, |payload| {
        AppEvent::CloudSyncConflict { payload }
    });
}

/// Emit and publish a file-transfer lifecycle event.
pub fn emit_transfer_event<T>(app: &tauri::AppHandle, payload: &T)
where
    T: Serialize,
{
    let _ = app.emit("transfer-event", payload);
    if let Ok(payload) = serde_json::to_value(payload) {
        publish_app_event(app, AppEvent::Transfer { payload });
    }
}

/// Emit and publish a terminal working-directory update.
pub fn emit_cwd_changed(app: &tauri::AppHandle, event_name: &str, session_id: &str, cwd: &str) {
    let _ = app.emit(event_name, cwd);
    publish_app_event(
        app,
        AppEvent::CwdChanged {
            session_id: session_id.to_string(),
            cwd: cwd.to_string(),
        },
    );
}

/// Emit and publish a terminal session close notification.
pub fn emit_session_closed(app: &tauri::AppHandle, session_id: &str) {
    let _ = app.emit(&format!("session-closed-{session_id}"), ());
    publish_app_event(
        app,
        AppEvent::SessionClosed {
            session_id: session_id.to_string(),
        },
    );
}

impl Default for AppEventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publishes_events_to_subscribers() {
        let bus = AppEventBus::with_capacity(4);
        let mut receiver = bus.subscribe();

        bus.publish_lossy(AppEvent::SessionsChanged);

        let event = receiver.recv().await.expect("event should be delivered");
        assert_eq!(event, AppEvent::SessionsChanged);
    }

    #[tokio::test]
    async fn preserves_terminal_event_payload() {
        let bus = AppEventBus::with_capacity(4);
        let mut receiver = bus.subscribe();

        bus.publish_lossy(AppEvent::TerminalOutput {
            session_id: "session-1".to_string(),
            data: "hello".to_string(),
        });

        let event = receiver.recv().await.expect("event should be delivered");
        assert_eq!(
            event,
            AppEvent::TerminalOutput {
                session_id: "session-1".to_string(),
                data: "hello".to_string(),
            }
        );
    }
}
