//! Core application facade shared by Tauri and future native UI shells.
//!
//! This facade centralizes construction of long-lived backend managers.  It is
//! intentionally UI-toolkit agnostic so the existing Tauri WebView frontend and
//! a future egui frontend can share the same runtime services during migration.

use std::sync::Arc;

use crate::app_event::AppEventBus;
use crate::core::ai::AgentApprovalManager;
use crate::core::sftp::TransferDuplicateManager;
use crate::core::ssh::{
    HostKeyVerifyManager, PendingAuthManager, PendingSshAuthManager, TunnelManager,
};
use crate::core::{CloudSyncManager, QuickCommandsStore, RecordingManager, SessionManager};

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
}

impl Default for NyatermCore {
    fn default() -> Self {
        Self::new()
    }
}
