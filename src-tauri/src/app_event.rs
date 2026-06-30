//! Typed application event bus for native UI frontends.
//!
//! The existing Tauri frontend consumes stringly-named window events.  Native
//! egui frontends need the same backend notifications without going through a
//! WebView, so this module provides a small typed bus that backend services can
//! adopt incrementally while the Tauri event bridge remains in place.

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
