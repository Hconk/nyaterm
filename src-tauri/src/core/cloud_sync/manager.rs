use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use tauri::{Emitter, Manager, async_runtime};
use tokio::sync::{Mutex, Notify};

use crate::app_event::{
    emit_cloud_sync_conflict, emit_cloud_sync_history_changed, emit_cloud_sync_status_changed,
};
use crate::config::{
    self, CloudConflictPreview, CloudSyncHistoryEntry, CloudSyncSettings, CloudSyncState,
    CloudSyncStatus,
};
use crate::error::{AppError, AppResult};

use super::crypto::{decrypt_snapshot_bytes, encrypt_snapshot_bytes, require_master_password};
use super::history_log::{log_history_entry, read_cloud_sync_history_from_logs};
use super::operator::{build_remote, ensure_remote_layout};
use super::remote::{
    RemoteSyncPointer, SYNC_CURRENT_FILE, SYNC_SNAPSHOTS_DIR, current_time_ms, elapsed_ms,
    is_legacy_sync_snapshot_path, legacy_sync_snapshot_file, load_sync_pointer, remote_path,
    write_sync_pointer,
};

use crate::core::portable_snapshot::{
    PortableSnapshot, PortableSnapshotKind, apply_portable_snapshot, build_portable_snapshot,
    decode_portable_snapshot, encode_portable_snapshot,
};

const CLOUD_SYNC_STARTUP_CHECK_TIMEOUT: Duration = Duration::from_secs(30);
const CLOUD_SYNC_OPERATION_TIMEOUT: Duration = Duration::from_secs(300);
const CLOUD_SYNC_QUICK_OPERATION_TIMEOUT: Duration = Duration::from_secs(60);
const CLOUD_SYNC_CLEANUP_TIMEOUT: Duration = Duration::from_secs(20);
const AUTOMATIC_RETRY_BACKOFF_MS: [u64; 4] = [60_000, 300_000, 900_000, 3_600_000];

pub struct CloudSyncManager {
    app_handle: OnceLock<tauri::AppHandle>,
    settings: Arc<Mutex<CloudSyncSettings>>,
    state: Arc<Mutex<CloudSyncState>>,
    status: Arc<Mutex<CloudSyncStatus>>,
    automatic_retry: Arc<Mutex<AutomaticRetryState>>,
    auto_push_notify: Arc<Notify>,
    auto_push_worker_started: AtomicBool,
    operation_lock: Arc<Mutex<()>>,
}

#[derive(Debug, Clone, Default)]
struct AutomaticRetryState {
    consecutive_failures: u32,
    blocked_until_ms: Option<u64>,
    suspended_until_settings_change: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutomaticRetryGate {
    Run,
    Wait(Duration),
    Suspended,
}

impl CloudSyncManager {
    pub fn new() -> Self {
        Self {
            app_handle: OnceLock::new(),
            settings: Arc::new(Mutex::new(CloudSyncSettings::default())),
            state: Arc::new(Mutex::new(CloudSyncState::default())),
            status: Arc::new(Mutex::new(CloudSyncStatus::default())),
            automatic_retry: Arc::new(Mutex::new(AutomaticRetryState::default())),
            auto_push_notify: Arc::new(Notify::new()),
            auto_push_worker_started: AtomicBool::new(false),
            operation_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn set_app_handle(&self, app: tauri::AppHandle) {
        let _ = self.app_handle.set(app);
    }

    pub async fn init(self: &Arc<Self>, app: tauri::AppHandle) -> AppResult<()> {
        self.set_app_handle(app.clone());

        {
            let settings = config::load_app_settings(&app)
                .map(|settings| settings.cloud_sync)
                .unwrap_or_default();
            *self.settings.lock().await = settings;
        }
        {
            let mut state = config::load_cloud_sync_state(&app).unwrap_or_default();
            if state.device_id.is_empty() {
                state.device_id = uuid::Uuid::new_v4().to_string();
                let _ = config::save_cloud_sync_state(&app, &state);
            }
            *self.state.lock().await = state;
        }
        self.ensure_auto_push_worker();

        self.set_status("idle", String::new(), None, None).await;

        let settings = self.settings.lock().await.clone();
        if settings.enabled && settings.auto_check_on_startup {
            if should_skip_startup_network_check(&settings) {
                self.set_status(
                    "idle",
                    "Startup cloud sync check skipped for GitHub Gist; sync will run on demand"
                        .to_string(),
                    None,
                    None,
                )
                .await;
                return Ok(());
            }

            let manager = Arc::clone(self);
            async_runtime::spawn(async move {
                if let Err(error) = with_operation_timeout(
                    "startup_check",
                    CLOUD_SYNC_STARTUP_CHECK_TIMEOUT,
                    manager.startup_check(),
                )
                .await
                {
                    manager.handle_startup_check_failure(error).await;
                }
            });
        }

        Ok(())
    }

    pub async fn replace_settings(&self, settings: CloudSyncSettings) -> AppResult<()> {
        let enabled = settings.enabled;
        let provider = settings.provider.clone();
        *self.settings.lock().await = settings.clone();
        self.reset_automatic_retry().await;
        self.set_status_after_settings_replace(enabled, provider)
            .await;
        Ok(())
    }

    pub async fn get_status(&self) -> CloudSyncStatus {
        self.status.lock().await.clone()
    }

    pub async fn list_history(&self) -> Vec<CloudSyncHistoryEntry> {
        let Ok(app) = self.app() else {
            return Vec::new();
        };
        read_cloud_sync_history_from_logs(&app).unwrap_or_default()
    }

    pub async fn notify_config_changed(self: &Arc<Self>) {
        let settings = self.settings.lock().await.clone();
        if !settings.enabled || !settings.auto_push_on_change {
            return;
        }
        self.auto_push_notify.notify_one();
    }

    pub async fn test_connection(&self) -> AppResult<()> {
        let started = Instant::now();
        let settings = self.settings.lock().await.clone();
        let result = with_operation_timeout(
            "test_connection",
            CLOUD_SYNC_QUICK_OPERATION_TIMEOUT,
            async {
                let _ = require_master_password()?;
                self.set_status(
                    "running",
                    "Connecting to cloud sync storage".to_string(),
                    Some("test_connection".to_string()),
                    None,
                )
                .await;
                let remote = trace_cloud_sync_step("test_connection", "build_remote", async {
                    self.build_remote_with_recovery(settings.clone()).await
                })
                .await?;
                self.set_status(
                    "running",
                    "Verifying cloud sync storage layout".to_string(),
                    Some("test_connection".to_string()),
                    None,
                )
                .await;
                trace_cloud_sync_step("test_connection", "ensure_remote_layout", async {
                    ensure_remote_layout(&remote, &settings.remote_root).await
                })
                .await?;
                self.set_status(
                    "running",
                    "Checking cloud sync storage access".to_string(),
                    Some("test_connection".to_string()),
                    None,
                )
                .await;
                let _ = trace_cloud_sync_step("test_connection", "exists_snapshots_dir", async {
                    remote
                        .exists(&remote_path(&settings.remote_root, SYNC_SNAPSHOTS_DIR))
                        .await
                })
                .await?;
                Ok::<(), AppError>(())
            },
        )
        .await;

        match result {
            Ok(()) => {
                self.reset_automatic_retry().await;
                self.append_history(CloudSyncHistoryEntry {
                    id: uuid::Uuid::new_v4().to_string(),
                    timestamp_ms: current_time_ms(),
                    kind: "sync".to_string(),
                    status: "success".to_string(),
                    trigger: "manual_test_connection".to_string(),
                    provider: Some(settings.provider),
                    revision: None,
                    duration_ms: Some(elapsed_ms(started.elapsed())),
                    message: "Cloud connection verified".to_string(),
                })
                .await;
                Ok(())
            }
            Err(error) => {
                self.record_failure("sync", "manual_test_connection", &error)
                    .await;
                Err(error)
            }
        }
    }

    pub async fn sync_push_now(self: &Arc<Self>, trigger: &str) -> AppResult<()> {
        match with_operation_timeout(
            trigger,
            CLOUD_SYNC_OPERATION_TIMEOUT,
            self.push_snapshot(trigger, false),
        )
        .await
        {
            Ok(()) => {
                self.reset_automatic_retry().await;
                Ok(())
            }
            Err(error) => {
                self.record_failure("sync", trigger, &error).await;
                Err(error)
            }
        }
    }

    pub async fn sync_pull_now(self: &Arc<Self>, trigger: &str) -> AppResult<()> {
        match with_operation_timeout(
            trigger,
            CLOUD_SYNC_OPERATION_TIMEOUT,
            self.pull_snapshot(trigger, false),
        )
        .await
        {
            Ok(()) => {
                self.reset_automatic_retry().await;
                Ok(())
            }
            Err(error) => {
                self.record_failure("sync", trigger, &error).await;
                Err(error)
            }
        }
    }

    pub async fn resolve_cloud_sync_conflict(self: &Arc<Self>, action: &str) -> AppResult<()> {
        let result = with_operation_timeout(action, CLOUD_SYNC_OPERATION_TIMEOUT, async {
            match action {
                "upload_local" => self.push_snapshot("resolve_upload", true).await,
                "download_remote" => self.pull_snapshot("resolve_download", true).await,
                _ => Err(AppError::Config(format!(
                    "Unsupported conflict resolution action '{}'",
                    action
                ))),
            }
        })
        .await;

        match result {
            Ok(()) => {
                self.reset_automatic_retry().await;
                Ok(())
            }
            Err(error) => {
                self.record_failure("sync", action, &error).await;
                Err(error)
            }
        }
    }

    async fn startup_check(self: &Arc<Self>) -> AppResult<()> {
        let Ok(_guard) = self.operation_lock.try_lock() else {
            self.set_status(
                "idle",
                "Startup cloud sync check skipped because another cloud sync operation is running"
                    .to_string(),
                None,
                None,
            )
            .await;
            tracing::info!("Startup cloud sync check skipped because another operation is running");
            return Ok(());
        };
        let settings = self.settings.lock().await.clone();
        if !settings.enabled {
            self.set_status("disabled", String::new(), None, None).await;
            return Ok(());
        }
        let _ = require_master_password()?;
        self.set_status(
            "running",
            "Connecting to cloud sync storage".to_string(),
            Some("startup_check".to_string()),
            None,
        )
        .await;
        let remote = trace_cloud_sync_step("startup_check", "build_remote", async {
            self.build_remote_with_recovery(settings.clone()).await
        })
        .await?;
        self.set_status(
            "running",
            "Verifying cloud sync storage layout".to_string(),
            Some("startup_check".to_string()),
            None,
        )
        .await;
        trace_cloud_sync_step("startup_check", "ensure_remote_layout", async {
            ensure_remote_layout(&remote, &settings.remote_root).await
        })
        .await?;

        let local_envelope = {
            let state = self.state.lock().await.clone();
            build_portable_snapshot(&self.app()?, PortableSnapshotKind::Sync, &state.device_id)?
        };
        let local_hash = local_envelope.payload_hash.clone();
        self.set_status(
            "running",
            "Reading latest cloud sync pointer".to_string(),
            Some("startup_check".to_string()),
            None,
        )
        .await;
        let latest = trace_cloud_sync_step("startup_check", "load_sync_pointer", async {
            load_sync_pointer(&remote, &settings.remote_root).await
        })
        .await?;

        {
            let mut state = self.state.lock().await;
            state.last_checked_at_ms = Some(current_time_ms());
            config::save_cloud_sync_state(&self.app()?, &state)?;
        }

        let Some(remote) = latest else {
            self.set_status(
                "idle",
                "No remote sync snapshot found".to_string(),
                None,
                None,
            )
            .await;
            return Ok(());
        };

        let state = self.state.lock().await.clone();
        let local_changed = state
            .last_synced_payload_hash
            .as_deref()
            .map_or(true, |hash| hash != local_hash);
        let remote_changed = state
            .last_applied_remote_revision
            .as_deref()
            .map_or(true, |revision| revision != remote.revision_id);

        if remote.payload_hash == local_hash {
            {
                let mut state = self.state.lock().await;
                state.last_synced_payload_hash = Some(local_hash);
                state.last_applied_remote_revision = Some(remote.revision_id.clone());
                state.last_checked_at_ms = Some(current_time_ms());
                config::save_cloud_sync_state(&self.app()?, &state)?;
            }
            self.set_status("idle", "Cloud sync is up to date".to_string(), None, None)
                .await;
            return Ok(());
        }

        if remote_changed && local_changed {
            let conflict = CloudConflictPreview {
                detected_at_ms: current_time_ms(),
                provider: settings.provider.clone(),
                local_payload_hash: local_hash,
                remote_payload_hash: remote.payload_hash.clone(),
                remote_revision: remote.revision_id.clone(),
                remote_created_at_ms: remote.created_at_ms,
                remote_device_id: remote.device_id.clone(),
                message: "Both local and cloud state changed since last sync".to_string(),
            };
            self.append_history(CloudSyncHistoryEntry {
                id: uuid::Uuid::new_v4().to_string(),
                timestamp_ms: current_time_ms(),
                kind: "sync".to_string(),
                status: "conflict".to_string(),
                trigger: "startup_check".to_string(),
                provider: Some(settings.provider.clone()),
                revision: Some(remote.revision_id.clone()),
                duration_ms: None,
                message: conflict.message.clone(),
            })
            .await;
            self.set_status("conflict", conflict.message.clone(), None, Some(conflict))
                .await;
            return Ok(());
        }

        if remote_changed {
            drop(_guard);
            return self.pull_snapshot("startup_check", false).await;
        }

        self.set_status("idle", "Local changes pending sync".to_string(), None, None)
            .await;
        Ok(())
    }

    async fn handle_startup_check_failure(&self, error: AppError) {
        if should_record_startup_check_failure(&error) {
            self.record_failure("sync", "startup_check", &error).await;
            tracing::warn!("Startup cloud sync check failed: {}", error);
            return;
        }

        let message = format!("Startup cloud sync check skipped: {error}");
        self.set_status("idle", message.clone(), None, None).await;
        tracing::warn!("{}", message);
    }

    fn ensure_auto_push_worker(self: &Arc<Self>) {
        if self.auto_push_worker_started.swap(true, Ordering::SeqCst) {
            return;
        }

        let manager = Arc::clone(self);
        async_runtime::spawn(async move {
            loop {
                manager.auto_push_notify.notified().await;

                loop {
                    let debounce_secs = manager.settings.lock().await.sync_debounce_seconds.max(1);
                    tokio::time::sleep(Duration::from_secs(debounce_secs)).await;

                    while tokio::time::timeout(
                        Duration::from_millis(100),
                        manager.auto_push_notify.notified(),
                    )
                    .await
                    .is_ok()
                    {}

                    loop {
                        let settings = manager.settings.lock().await.clone();
                        if !settings.enabled || !settings.auto_push_on_change {
                            break;
                        }
                        match manager.automatic_retry_gate().await {
                            AutomaticRetryGate::Run => {}
                            AutomaticRetryGate::Wait(delay) => {
                                tokio::time::sleep(delay).await;
                                continue;
                            }
                            AutomaticRetryGate::Suspended => break,
                        }
                        if let Err(error) = manager.sync_push_now("auto_push").await {
                            tracing::warn!("Auto push failed: {}", error);
                            match manager.automatic_retry_gate().await {
                                AutomaticRetryGate::Wait(delay) => {
                                    tokio::time::sleep(delay).await;
                                    continue;
                                }
                                AutomaticRetryGate::Suspended | AutomaticRetryGate::Run => break,
                            }
                        }
                        break;
                    }

                    let pending_more = tokio::time::timeout(
                        Duration::from_millis(100),
                        manager.auto_push_notify.notified(),
                    )
                    .await
                    .is_ok();
                    if !pending_more {
                        break;
                    }
                }
            }
        });
    }

    async fn push_snapshot(self: &Arc<Self>, trigger: &str, force: bool) -> AppResult<()> {
        let _guard = self.operation_lock.lock().await;
        let _ = require_master_password()?;
        let settings = self.settings.lock().await.clone();
        if !settings.enabled {
            return Err(AppError::Config(
                "Cloud sync is disabled in settings".to_string(),
            ));
        }

        self.set_status(
            "running",
            "Uploading cloud sync snapshot".to_string(),
            Some("sync_push".to_string()),
            None,
        )
        .await;

        let started = Instant::now();
        let state_snapshot = self.state.lock().await.clone();
        self.set_status(
            "running",
            "Connecting to cloud sync storage".to_string(),
            Some("sync_push".to_string()),
            None,
        )
        .await;
        let remote = trace_cloud_sync_step(trigger, "build_remote", async {
            self.build_remote_with_recovery(settings.clone()).await
        })
        .await?;
        self.set_status(
            "running",
            "Verifying cloud sync storage layout".to_string(),
            Some("sync_push".to_string()),
            None,
        )
        .await;
        trace_cloud_sync_step(trigger, "ensure_remote_layout", async {
            ensure_remote_layout(&remote, &settings.remote_root).await
        })
        .await?;

        self.set_status(
            "running",
            "Preparing local cloud sync snapshot".to_string(),
            Some("sync_push".to_string()),
            None,
        )
        .await;
        let envelope = build_portable_snapshot(
            &self.app()?,
            PortableSnapshotKind::Sync,
            &state_snapshot.device_id,
        )?;
        let local_hash = envelope.payload_hash.clone();
        self.set_status(
            "running",
            "Reading latest cloud sync pointer".to_string(),
            Some("sync_push".to_string()),
            None,
        )
        .await;
        let latest = trace_cloud_sync_step(trigger, "load_sync_pointer", async {
            load_sync_pointer(&remote, &settings.remote_root).await
        })
        .await?;

        if let Some(remote) = &latest {
            if remote.payload_hash == local_hash {
                {
                    let mut state = self.state.lock().await;
                    state.last_synced_payload_hash = Some(local_hash);
                    state.last_applied_remote_revision = Some(remote.revision_id.clone());
                    state.last_checked_at_ms = Some(current_time_ms());
                    config::save_cloud_sync_state(&self.app()?, &state)?;
                }
                self.set_status(
                    "idle",
                    "Cloud sync is already up to date".to_string(),
                    None,
                    None,
                )
                .await;
                return Ok(());
            }
        }

        let remote_changed = latest.as_ref().is_some_and(|remote| {
            state_snapshot
                .last_applied_remote_revision
                .as_deref()
                .map_or(true, |revision| revision != remote.revision_id)
        });
        let local_changed = state_snapshot
            .last_synced_payload_hash
            .as_deref()
            .map_or(true, |hash| hash != local_hash);

        if remote_changed && !force {
            if local_changed {
                let remote = latest.expect("checked above");
                let conflict = CloudConflictPreview {
                    detected_at_ms: current_time_ms(),
                    provider: settings.provider.clone(),
                    local_payload_hash: local_hash.clone(),
                    remote_payload_hash: remote.payload_hash.clone(),
                    remote_revision: remote.revision_id.clone(),
                    remote_created_at_ms: remote.created_at_ms,
                    remote_device_id: remote.device_id.clone(),
                    message: "Both local and cloud state changed since last sync".to_string(),
                };
                self.append_history(CloudSyncHistoryEntry {
                    id: uuid::Uuid::new_v4().to_string(),
                    timestamp_ms: current_time_ms(),
                    kind: "sync".to_string(),
                    status: "conflict".to_string(),
                    trigger: trigger.to_string(),
                    provider: Some(settings.provider.clone()),
                    revision: Some(remote.revision_id.clone()),
                    duration_ms: Some(elapsed_ms(started.elapsed())),
                    message: conflict.message.clone(),
                })
                .await;
                self.set_status("conflict", conflict.message.clone(), None, Some(conflict))
                    .await;
                return Err(AppError::Config("Cloud sync conflict detected".to_string()));
            }
            return Err(AppError::Config(
                "Remote snapshot is newer than local state. Pull first.".to_string(),
            ));
        }

        self.set_status(
            "running",
            "Uploading cloud sync snapshot".to_string(),
            Some("sync_push".to_string()),
            None,
        )
        .await;
        trace_cloud_sync_step(trigger, "write_current_sync_snapshot", async {
            write_current_sync_snapshot(&remote, &settings.remote_root, &envelope).await
        })
        .await?;

        let pointer = RemoteSyncPointer {
            revision_id: envelope.revision_id.clone(),
            created_at_ms: envelope.created_at_ms,
            payload_hash: envelope.payload_hash.clone(),
            device_id: envelope.device_id.clone(),
            app_version: envelope.app_version.clone(),
        };
        self.set_status(
            "running",
            "Updating cloud sync pointer".to_string(),
            Some("sync_push".to_string()),
            None,
        )
        .await;
        trace_cloud_sync_step(trigger, "write_sync_pointer", async {
            write_sync_pointer(&remote, &settings.remote_root, &pointer).await
        })
        .await?;
        schedule_cleanup_legacy_sync_snapshots(remote.clone(), settings.remote_root.clone());

        {
            let mut state = self.state.lock().await;
            state.last_synced_payload_hash = Some(envelope.payload_hash.clone());
            state.last_applied_remote_revision = Some(envelope.revision_id.clone());
            state.last_synced_at_ms = Some(current_time_ms());
            state.last_checked_at_ms = Some(current_time_ms());
            config::save_cloud_sync_state(&self.app()?, &state)?;
        }

        self.append_history(CloudSyncHistoryEntry {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp_ms: current_time_ms(),
            kind: "sync".to_string(),
            status: "success".to_string(),
            trigger: trigger.to_string(),
            provider: Some(settings.provider.clone()),
            revision: Some(envelope.revision_id.clone()),
            duration_ms: Some(elapsed_ms(started.elapsed())),
            message: "Cloud sync snapshot uploaded".to_string(),
        })
        .await;
        self.set_status(
            "idle",
            "Cloud sync snapshot uploaded".to_string(),
            None,
            None,
        )
        .await;
        Ok(())
    }

    async fn pull_snapshot(self: &Arc<Self>, trigger: &str, force: bool) -> AppResult<()> {
        let _guard = self.operation_lock.lock().await;
        let _ = require_master_password()?;
        let settings = self.settings.lock().await.clone();
        if !settings.enabled {
            return Err(AppError::Config(
                "Cloud sync is disabled in settings".to_string(),
            ));
        }

        self.set_status(
            "running",
            "Downloading cloud sync snapshot".to_string(),
            Some("sync_pull".to_string()),
            None,
        )
        .await;

        let started = Instant::now();
        self.set_status(
            "running",
            "Connecting to cloud sync storage".to_string(),
            Some("sync_pull".to_string()),
            None,
        )
        .await;
        let remote = trace_cloud_sync_step(trigger, "build_remote", async {
            self.build_remote_with_recovery(settings.clone()).await
        })
        .await?;
        self.set_status(
            "running",
            "Reading latest cloud sync pointer".to_string(),
            Some("sync_pull".to_string()),
            None,
        )
        .await;
        let latest = trace_cloud_sync_step(trigger, "load_sync_pointer", async {
            load_sync_pointer(&remote, &settings.remote_root).await
        })
        .await?
        .ok_or_else(|| AppError::Config("No remote sync snapshot found".to_string()))?;

        let state_snapshot = self.state.lock().await.clone();
        self.set_status(
            "running",
            "Preparing local cloud sync snapshot".to_string(),
            Some("sync_pull".to_string()),
            None,
        )
        .await;
        let local_envelope = build_portable_snapshot(
            &self.app()?,
            PortableSnapshotKind::Sync,
            &state_snapshot.device_id,
        )?;
        let local_changed = state_snapshot
            .last_synced_payload_hash
            .as_deref()
            .map_or(true, |hash| hash != local_envelope.payload_hash);
        let remote_changed = state_snapshot
            .last_applied_remote_revision
            .as_deref()
            .map_or(true, |revision| revision != latest.revision_id);

        if latest.payload_hash == local_envelope.payload_hash {
            {
                let mut state = self.state.lock().await;
                state.last_synced_payload_hash = Some(latest.payload_hash.clone());
                state.last_applied_remote_revision = Some(latest.revision_id.clone());
                state.last_checked_at_ms = Some(current_time_ms());
                config::save_cloud_sync_state(&self.app()?, &state)?;
            }
            self.set_status(
                "idle",
                "Cloud sync is already up to date".to_string(),
                None,
                None,
            )
            .await;
            return Ok(());
        }

        if remote_changed && local_changed && !force {
            let conflict = CloudConflictPreview {
                detected_at_ms: current_time_ms(),
                provider: settings.provider.clone(),
                local_payload_hash: local_envelope.payload_hash.clone(),
                remote_payload_hash: latest.payload_hash.clone(),
                remote_revision: latest.revision_id.clone(),
                remote_created_at_ms: latest.created_at_ms,
                remote_device_id: latest.device_id.clone(),
                message: "Both local and cloud state changed since last sync".to_string(),
            };
            self.append_history(CloudSyncHistoryEntry {
                id: uuid::Uuid::new_v4().to_string(),
                timestamp_ms: current_time_ms(),
                kind: "sync".to_string(),
                status: "conflict".to_string(),
                trigger: trigger.to_string(),
                provider: Some(settings.provider.clone()),
                revision: Some(latest.revision_id.clone()),
                duration_ms: Some(elapsed_ms(started.elapsed())),
                message: conflict.message.clone(),
            })
            .await;
            self.set_status("conflict", conflict.message.clone(), None, Some(conflict))
                .await;
            return Err(AppError::Config("Cloud sync conflict detected".to_string()));
        }

        if !remote_changed && !force {
            return Err(AppError::Config(
                "No newer remote sync snapshot is available".to_string(),
            ));
        }

        self.set_status(
            "running",
            "Downloading cloud sync snapshot".to_string(),
            Some("sync_pull".to_string()),
            None,
        )
        .await;
        let envelope = trace_cloud_sync_step(trigger, "read_sync_snapshot", async {
            read_sync_snapshot(&remote, &settings.remote_root, &latest).await
        })
        .await?;
        self.set_status(
            "running",
            "Applying cloud sync snapshot".to_string(),
            Some("sync_pull".to_string()),
            None,
        )
        .await;
        trace_cloud_sync_step(trigger, "apply_portable_snapshot", async {
            apply_portable_snapshot(&self.app()?, &envelope).await
        })
        .await?;
        self.set_status(
            "running",
            "Refreshing cloud sync current snapshot".to_string(),
            Some("sync_pull".to_string()),
            None,
        )
        .await;
        trace_cloud_sync_step(trigger, "write_current_sync_snapshot", async {
            write_current_sync_snapshot(&remote, &settings.remote_root, &envelope).await
        })
        .await?;
        schedule_cleanup_legacy_sync_snapshots(remote.clone(), settings.remote_root.clone());

        {
            let mut state = self.state.lock().await;
            state.last_synced_payload_hash = Some(envelope.payload_hash.clone());
            state.last_applied_remote_revision = Some(envelope.revision_id.clone());
            state.last_synced_at_ms = Some(current_time_ms());
            state.last_checked_at_ms = Some(current_time_ms());
            config::save_cloud_sync_state(&self.app()?, &state)?;
        }

        self.append_history(CloudSyncHistoryEntry {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp_ms: current_time_ms(),
            kind: "sync".to_string(),
            status: "success".to_string(),
            trigger: trigger.to_string(),
            provider: Some(settings.provider.clone()),
            revision: Some(envelope.revision_id.clone()),
            duration_ms: Some(elapsed_ms(started.elapsed())),
            message: "Cloud sync snapshot downloaded".to_string(),
        })
        .await;
        self.set_status(
            "idle",
            "Cloud sync snapshot downloaded".to_string(),
            None,
            None,
        )
        .await;
        Ok(())
    }

    async fn append_history(&self, entry: CloudSyncHistoryEntry) {
        let Ok(app) = self.app() else {
            return;
        };
        log_history_entry(&entry);
        let snapshot = read_cloud_sync_history_from_logs(&app).unwrap_or_default();
        emit_cloud_sync_history_changed(&app, &snapshot);
    }

    async fn record_failure(&self, kind: &str, trigger: &str, error: &AppError) {
        let status = self.status.lock().await.clone();
        if status.state == "conflict" {
            return;
        }

        let provider = self.settings.lock().await.provider.clone();
        let message = error.to_string();

        self.append_history(CloudSyncHistoryEntry {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp_ms: current_time_ms(),
            kind: kind.to_string(),
            status: "failed".to_string(),
            trigger: trigger.to_string(),
            provider: Some(provider),
            revision: None,
            duration_ms: None,
            message: message.clone(),
        })
        .await;
        self.set_status("failed", message, None, None).await;
        self.record_automatic_retry_failure(trigger, error).await;
    }

    async fn reset_automatic_retry(&self) {
        *self.automatic_retry.lock().await = AutomaticRetryState::default();
    }

    async fn automatic_retry_gate(&self) -> AutomaticRetryGate {
        let retry = self.automatic_retry.lock().await.clone();
        if retry.suspended_until_settings_change {
            return AutomaticRetryGate::Suspended;
        }
        let Some(blocked_until_ms) = retry.blocked_until_ms else {
            return AutomaticRetryGate::Run;
        };
        let now = current_time_ms();
        if blocked_until_ms <= now {
            return AutomaticRetryGate::Run;
        }
        AutomaticRetryGate::Wait(Duration::from_millis(blocked_until_ms.saturating_sub(now)))
    }

    async fn record_automatic_retry_failure(&self, trigger: &str, error: &AppError) {
        if !is_automatic_trigger(trigger) {
            return;
        }

        let mut retry = self.automatic_retry.lock().await;
        if is_non_retryable_automatic_error(error) {
            retry.suspended_until_settings_change = true;
            retry.blocked_until_ms = None;
            return;
        }

        retry.consecutive_failures = retry.consecutive_failures.saturating_add(1);
        let index = retry
            .consecutive_failures
            .saturating_sub(1)
            .min((AUTOMATIC_RETRY_BACKOFF_MS.len() - 1) as u32) as usize;
        retry.blocked_until_ms =
            Some(current_time_ms().saturating_add(AUTOMATIC_RETRY_BACKOFF_MS[index]));
    }

    async fn build_remote_with_recovery(
        &self,
        mut settings: CloudSyncSettings,
    ) -> AppResult<super::operator::CloudRemote> {
        if settings.provider != "github_gist" {
            return build_remote(&settings);
        }

        let Some(access_token) = settings
            .github_gist
            .access_token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return build_remote(&settings);
        };

        let existing_gist_id = settings.github_gist.gist_id.clone();
        let resolved_gist_id = super::github_gist_auth::resolve_github_gist_id(
            access_token,
            Some(existing_gist_id.clone()),
        )
        .await?;

        if resolved_gist_id != existing_gist_id.trim() {
            tracing::warn!(
                old_gist_id = %existing_gist_id,
                new_gist_id = %resolved_gist_id,
                "GitHub Gist sync storage was missing; created a replacement gist"
            );
            settings.github_gist.gist_id = resolved_gist_id;
            self.persist_recovered_settings(settings.clone()).await?;
            self.refresh_status_after_recovered_settings(&settings)
                .await;
        }

        build_remote(&settings)
    }

    async fn persist_recovered_settings(&self, settings: CloudSyncSettings) -> AppResult<()> {
        *self.settings.lock().await = settings.clone();

        let app = self.app()?;
        let mut app_settings = config::load_app_settings(&app)?;
        app_settings.cloud_sync = settings;

        let mut persisted_settings = app_settings.clone();
        persisted_settings.cloud_sync =
            config::encrypt_cloud_sync_settings(persisted_settings.cloud_sync)?;
        persisted_settings.ai = config::encrypt_ai_settings(persisted_settings.ai)?;
        config::save_app_settings(&app, &persisted_settings)?;

        let _ = app.emit("settings-changed", ());
        crate::tray::schedule_refresh(&app);
        Ok(())
    }

    async fn refresh_status_after_recovered_settings(&self, settings: &CloudSyncSettings) {
        let app = self.app().ok();
        let state = self.state.lock().await.clone();
        let status = {
            let mut status = self.status.lock().await;
            status.enabled = settings.enabled;
            status.provider = settings.provider.clone();
            status.last_checked_at_ms = state.last_checked_at_ms;
            status.last_synced_at_ms = state.last_synced_at_ms;
            status.clone()
        };
        if let Some(app) = app {
            emit_cloud_sync_status_changed(&app, &status);
            crate::tray::schedule_refresh(&app);
        }
    }

    async fn set_status(
        &self,
        state_value: &str,
        message: String,
        current_operation: Option<String>,
        conflict: Option<CloudConflictPreview>,
    ) {
        let app = self.app().ok();
        let settings = self.settings.lock().await.clone();
        let state = self.state.lock().await.clone();
        let status = CloudSyncStatus {
            enabled: settings.enabled,
            provider: settings.provider.clone(),
            state: state_value.to_string(),
            message,
            current_operation,
            last_checked_at_ms: state.last_checked_at_ms,
            last_synced_at_ms: state.last_synced_at_ms,
            conflict: conflict.clone(),
        };
        *self.status.lock().await = status.clone();
        if let Some(app) = app {
            emit_cloud_sync_status_changed(&app, &status);
            emit_cloud_sync_conflict(&app, &conflict);
            crate::tray::schedule_refresh(&app);
        }
    }

    async fn set_status_after_settings_replace(&self, enabled: bool, provider: String) {
        let app = self.app().ok();
        let status = {
            let mut status = self.status.lock().await;
            status.enabled = enabled;
            status.provider = provider;
            status.state = if enabled { "idle" } else { "disabled" }.to_string();
            status.message.clear();
            status.current_operation = None;
            status.conflict = None;
            status.clone()
        };
        if let Some(app) = app {
            emit_cloud_sync_status_changed(&app, &status);
            emit_cloud_sync_conflict(&app, &Option::<CloudConflictPreview>::None);
            crate::tray::schedule_refresh(&app);
        }
    }

    fn app(&self) -> AppResult<tauri::AppHandle> {
        self.app_handle
            .get()
            .cloned()
            .ok_or_else(|| AppError::Config("cloud sync app handle is not initialized".to_string()))
    }
}

async fn with_operation_timeout<T, F>(
    operation: &str,
    timeout_duration: Duration,
    future: F,
) -> AppResult<T>
where
    F: Future<Output = AppResult<T>>,
{
    tokio::time::timeout(timeout_duration, future)
        .await
        .map_err(|_| {
            AppError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "Cloud sync operation '{}' timed out after {} seconds",
                    operation,
                    timeout_duration.as_secs()
                ),
            ))
        })?
}

async fn trace_cloud_sync_step<T, F>(operation: &str, step: &str, future: F) -> AppResult<T>
where
    F: Future<Output = AppResult<T>>,
{
    tracing::info!(operation, step, "Cloud sync operation step started");
    let started = Instant::now();
    let result = future.await;
    let duration_ms = elapsed_ms(started.elapsed());
    match &result {
        Ok(_) => tracing::info!(
            operation,
            step,
            duration_ms,
            "Cloud sync operation step completed"
        ),
        Err(error) => tracing::warn!(
            operation,
            step,
            duration_ms,
            error = %error,
            "Cloud sync operation step failed"
        ),
    }
    result
}

async fn write_current_sync_snapshot(
    remote: &super::operator::CloudRemote,
    remote_root: &str,
    envelope: &PortableSnapshot,
) -> AppResult<()> {
    let encoded = encode_portable_snapshot(envelope)?;
    let encrypted = encrypt_snapshot_bytes(&encoded)?;
    remote
        .write(&remote_path(remote_root, SYNC_CURRENT_FILE), encrypted)
        .await
}

async fn read_sync_snapshot(
    remote: &super::operator::CloudRemote,
    remote_root: &str,
    pointer: &RemoteSyncPointer,
) -> AppResult<PortableSnapshot> {
    if let Some(raw) = remote
        .read_if_exists(&remote_path(remote_root, SYNC_CURRENT_FILE))
        .await?
    {
        let envelope = decode_remote_sync_snapshot(&raw)?;
        if envelope.revision_id == pointer.revision_id {
            return Ok(envelope);
        }

        tracing::warn!(
            current_revision = %envelope.revision_id,
            pointer_revision = %pointer.revision_id,
            "Cloud sync current snapshot revision differs from latest pointer; trying legacy revision path"
        );
    }

    let legacy_path = remote_path(
        remote_root,
        &legacy_sync_snapshot_file(&pointer.revision_id),
    );
    let raw = remote.read(&legacy_path).await?;
    decode_remote_sync_snapshot(&raw)
}

fn decode_remote_sync_snapshot(raw: &[u8]) -> AppResult<PortableSnapshot> {
    let decrypted = decrypt_snapshot_bytes(raw)?;
    decode_portable_snapshot(&decrypted)
}

fn schedule_cleanup_legacy_sync_snapshots(
    remote: super::operator::CloudRemote,
    remote_root: String,
) {
    async_runtime::spawn(async move {
        let result = with_operation_timeout(
            "cleanup_legacy_sync_snapshots",
            CLOUD_SYNC_CLEANUP_TIMEOUT,
            async {
                cleanup_legacy_sync_snapshots(&remote, &remote_root).await;
                Ok(())
            },
        )
        .await;

        if let Err(error) = result {
            tracing::warn!(
                error = %error,
                "Legacy cloud sync snapshot cleanup did not complete"
            );
        }
    });
}

async fn cleanup_legacy_sync_snapshots(remote: &super::operator::CloudRemote, remote_root: &str) {
    let prefix = remote_path(remote_root, SYNC_SNAPSHOTS_DIR);
    let paths = match trace_cloud_sync_step(
        "cleanup_legacy_sync_snapshots",
        "list_legacy_snapshots",
        remote.list_files(&prefix),
    )
    .await
    {
        Ok(paths) => paths,
        Err(error) => {
            tracing::warn!("Failed to list legacy cloud sync snapshots: {}", error);
            return;
        }
    };

    for path in paths
        .into_iter()
        .filter(|path| is_legacy_sync_snapshot_path(path, remote_root))
    {
        if let Err(error) = trace_cloud_sync_step(
            "cleanup_legacy_sync_snapshots",
            "delete_legacy_snapshot",
            remote.delete(&path),
        )
        .await
        {
            tracing::warn!(
                path = %path,
                error = %error,
                "Failed to delete legacy cloud sync snapshot"
            );
        }
    }
}

fn is_automatic_trigger(trigger: &str) -> bool {
    matches!(trigger, "auto_push" | "startup_check")
}

fn is_non_retryable_automatic_error(error: &AppError) -> bool {
    matches!(error, AppError::Auth(_) | AppError::Config(_))
}

fn should_record_startup_check_failure(error: &AppError) -> bool {
    !matches!(error, AppError::Io(_))
}

fn should_skip_startup_network_check(settings: &CloudSyncSettings) -> bool {
    settings.provider == "github_gist"
}

pub async fn notify_config_changed(app: &tauri::AppHandle) {
    let manager = app.state::<Arc<CloudSyncManager>>();
    manager.inner().notify_config_changed().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CloudSyncSettings, S3SyncSettings, WebdavSyncSettings};

    #[test]
    fn default_manager_constructs() {
        let _ = CloudSyncManager::new();
    }

    #[test]
    fn cloud_sync_settings_support_both_provider_shapes() {
        let mut settings = CloudSyncSettings::default();
        settings.provider = "webdav".to_string();
        settings.webdav = WebdavSyncSettings {
            endpoint: "https://dav.example.com".to_string(),
            root: "/nyaterm".to_string(),
            username: "user".to_string(),
            password: Some("cipher".to_string()),
        };
        settings.s3 = S3SyncSettings {
            endpoint: "https://s3.example.com".to_string(),
            bucket: "bucket".to_string(),
            region: "auto".to_string(),
            root: "/nyaterm".to_string(),
            access_key_id: Some("cipher".to_string()),
            secret_access_key: Some("cipher".to_string()),
            session_token: None,
            virtual_host_style: true,
        };

        assert_eq!(settings.provider, "webdav");
        assert!(settings.webdav.password.is_some());
        assert!(settings.s3.secret_access_key.is_some());
    }

    #[test]
    fn automatic_retry_classifies_auth_and_config_as_non_retryable() {
        assert!(is_non_retryable_automatic_error(&AppError::Auth(
            "bad credentials".to_string()
        )));
        assert!(is_non_retryable_automatic_error(&AppError::Config(
            "invalid endpoint".to_string()
        )));
        assert!(!is_non_retryable_automatic_error(&AppError::Io(
            std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout")
        )));
    }

    #[test]
    fn automatic_trigger_detection_is_limited_to_background_work() {
        assert!(is_automatic_trigger("auto_push"));
        assert!(is_automatic_trigger("startup_check"));
        assert!(!is_automatic_trigger("manual_push"));
        assert!(!is_automatic_trigger("manual_test_connection"));
    }

    #[tokio::test]
    async fn record_failure_clears_current_operation() {
        let manager = CloudSyncManager::new();
        *manager.status.lock().await = CloudSyncStatus {
            state: "running".to_string(),
            current_operation: Some("sync_push".to_string()),
            ..CloudSyncStatus::default()
        };

        manager
            .record_failure(
                "sync",
                "manual_push",
                &AppError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout")),
            )
            .await;

        let status = manager.status.lock().await.clone();
        assert_eq!(status.state, "failed");
        assert!(status.current_operation.is_none());
    }

    #[tokio::test]
    async fn status_update_after_state_save_does_not_deadlock() {
        let manager = CloudSyncManager::new();

        {
            let mut state = manager.state.lock().await;
            state.last_checked_at_ms = Some(current_time_ms());
        }

        tokio::time::timeout(
            Duration::from_millis(100),
            manager.set_status(
                "idle",
                "Cloud sync is already up to date".to_string(),
                None,
                None,
            ),
        )
        .await
        .expect("set_status should not wait on a retained state lock");

        let status = manager.status.lock().await.clone();
        assert_eq!(status.state, "idle");
        assert_eq!(status.message, "Cloud sync is already up to date");
    }

    #[tokio::test]
    async fn automatic_timeout_failure_uses_retry_backoff() {
        let manager = CloudSyncManager::new();
        let error = AppError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout"));

        manager
            .record_automatic_retry_failure("auto_push", &error)
            .await;

        let retry = manager.automatic_retry.lock().await.clone();
        assert!(!retry.suspended_until_settings_change);
        assert!(retry.blocked_until_ms.is_some());
    }

    #[tokio::test]
    async fn startup_check_skips_when_operation_lock_is_busy() {
        let manager = Arc::new(CloudSyncManager::new());
        let _guard = manager.operation_lock.lock().await;

        manager
            .startup_check()
            .await
            .expect("busy startup check should skip cleanly");

        let status = manager.status.lock().await.clone();
        assert_eq!(status.state, "idle");
        assert!(status.message.contains("skipped"));
    }

    #[tokio::test]
    async fn startup_io_failure_does_not_use_failure_history_or_retry_backoff() {
        let manager = CloudSyncManager::new();
        let error = AppError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout"));

        manager.handle_startup_check_failure(error).await;

        let status = manager.status.lock().await.clone();
        assert_eq!(status.state, "idle");
        assert!(status.message.contains("skipped"));

        let retry = manager.automatic_retry.lock().await.clone();
        assert_eq!(retry.consecutive_failures, 0);
        assert!(retry.blocked_until_ms.is_none());
        assert!(!retry.suspended_until_settings_change);
    }

    #[test]
    fn startup_check_records_only_non_io_failures() {
        assert!(!should_record_startup_check_failure(&AppError::Io(
            std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout")
        )));
        assert!(should_record_startup_check_failure(&AppError::Config(
            "bad cloud sync config".to_string()
        )));
        assert!(should_record_startup_check_failure(&AppError::Auth(
            "bad credentials".to_string()
        )));
    }

    #[test]
    fn github_gist_skips_startup_network_check() {
        let mut settings = CloudSyncSettings::default();
        settings.provider = "github_gist".to_string();
        assert!(should_skip_startup_network_check(&settings));

        settings.provider = "webdav".to_string();
        assert!(!should_skip_startup_network_check(&settings));
    }
}
