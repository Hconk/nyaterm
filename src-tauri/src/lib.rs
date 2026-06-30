#![recursion_limit = "256"]

//! Tauri app entry point: state construction, plugin registration, and command routing.

mod app;
mod app_core;
mod app_event;
mod cmd;
mod config;
mod core;
mod error;
mod observability;
mod platform;
mod runtime;
mod storage;
mod tray;
mod utils;
mod window_state;

use crate::app_core::NyatermCore;
use crate::cmd::app::AppLockState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let runtime = runtime::resolve().expect("failed to resolve runtime paths");
    runtime::prepare_webview_environment(&runtime);

    let core = NyatermCore::new();
    let session_manager = core.session_manager.clone();
    let tunnel_manager = core.tunnel_manager.clone();
    let recording_manager = core.recording_manager.clone();
    let pending_auth_manager = core.pending_auth_manager.clone();
    let pending_ssh_auth_manager = core.pending_ssh_auth_manager.clone();
    let host_key_verify_manager = core.host_key_verify_manager.clone();
    let quick_commands_store = core.quick_commands_store.clone();
    let cloud_sync_manager = core.cloud_sync_manager.clone();
    let agent_approval_manager = core.agent_approval_manager.clone();
    let transfer_duplicate_manager = core.transfer_duplicate_manager.clone();
    let event_bus = core.event_bus.clone();
    let app_lock_state = AppLockState::default();

    let builder = tauri::Builder::default();
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
        if let Err(error) = app::create_additional_main_window(app) {
            tracing::warn!("Failed to create additional main window: {}", error);
            app::show_main_window(app);
        }
    }));
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let builder = builder.plugin(tauri_plugin_updater::Builder::new().build());

    let runtime_for_setup = runtime.clone();
    let mut context = tauri::generate_context!();
    runtime::apply_to_context(&mut context, &runtime);

    builder
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(session_manager.clone())
        .manage(tunnel_manager.clone())
        .manage(recording_manager.clone())
        .manage(pending_auth_manager.clone())
        .manage(pending_ssh_auth_manager.clone())
        .manage(host_key_verify_manager.clone())
        .manage(quick_commands_store.clone())
        .manage(cloud_sync_manager.clone())
        .manage(agent_approval_manager.clone())
        .manage(transfer_duplicate_manager.clone())
        .manage(event_bus)
        .manage(core)
        .manage(app_lock_state)
        .setup(move |a| {
            app::setup(
                a,
                session_manager,
                quick_commands_store,
                cloud_sync_manager,
                runtime_for_setup.clone(),
            )
        })
        .on_window_event(app::on_window_event)
        .invoke_handler(tauri::generate_handler![
            cmd::app::quit_application,
            cmd::app::hide_main_window,
            cmd::app::open_download_dir,
            cmd::app::open_log_dir,
            cmd::app::get_app_runtime_info,
            cmd::app::get_app_lock_state,
            cmd::app::set_app_lock_state,
            cmd::app::open_child_window,
            cmd::app::open_transfer_target_directory,
            cmd::app::resolve_local_drop_paths,
            cmd::app::read_background_image_data_url,
            cmd::ai::start_ai_chat_stream,
            cmd::ai::list_ai_model_names,
            cmd::ai::cancel_ai_chat_stream,
            cmd::ai::respond_agent_step,
            cmd::ai::get_ai_sessions,
            cmd::ai::get_ai_messages,
            cmd::ai::clear_ai_history,
            cmd::ai::delete_ai_session,
            cmd::ai::append_ai_audit,
            cmd::ai::get_ai_audit_logs,
            cmd::clipboard::read_clipboard_text,
            cmd::clipboard::read_clipboard_path_payload,
            cmd::clipboard::upload_clipboard_image_to_ssh,
            cmd::log::append_frontend_logs,
            cmd::log::export_diagnostics,
            cmd::settings::get_system_fonts,
            cmd::settings::get_system_font_infos,
            cmd::cloud_sync::test_cloud_sync_connection,
            cmd::cloud_sync::get_cloud_sync_status,
            cmd::cloud_sync::sync_push_now,
            cmd::cloud_sync::sync_pull_now,
            cmd::cloud_sync::resolve_cloud_sync_conflict,
            cmd::cloud_sync::list_cloud_sync_history,
            cmd::cloud_sync::begin_github_gist_device_flow,
            cmd::cloud_sync::poll_github_gist_device_flow,
            cmd::cloud_sync::cancel_github_gist_device_flow,
            cmd::session::create_ssh_session,
            cmd::session::create_multiplexed_ssh_session,
            cmd::session::create_local_session,
            cmd::session::create_telnet_session,
            cmd::session::create_serial_session,
            cmd::session::cancel_session_creation,
            cmd::session::list_serial_ports,
            cmd::session::write_to_session,
            cmd::session::set_session_output_paused,
            cmd::session::resize_session,
            cmd::session::attach_session,
            cmd::session::close_session,
            cmd::session::list_sessions,
            cmd::session::add_command_history,
            cmd::session::register_command_submission,
            cmd::session::get_command_history,
            cmd::session::delete_command_history,
            cmd::session::fuzzy_search_history,
            cmd::session::fuzzy_search_commands,
            cmd::session::fuzzy_search_candidates,
            cmd::session::start_recording,
            cmd::session::stop_recording,
            cmd::session::is_recording,
            cmd::session::save_session_transcript,
            cmd::session::terminal_history_search,
            cmd::session::list_recording_sessions,
            cmd::session::set_recording_memory_limit,
            cmd::session::submit_otp_response,
            cmd::session::cancel_otp_request,
            cmd::session::submit_ssh_auth_response,
            cmd::session::cancel_ssh_auth_request,
            cmd::session::respond_host_key_verify,
            cmd::session::zmodem_accept_download,
            cmd::session::zmodem_accept_upload,
            cmd::session::zmodem_cancel,
            cmd::sftp::get_home_dir,
            cmd::sftp::list_remote_dir,
            cmd::sftp::delete_remote_file,
            cmd::sftp::rename_remote_file,
            cmd::sftp::sanitize_download_file_name,
            cmd::sftp::download_remote_file,
            cmd::sftp::upload_local_file,
            cmd::sftp::get_file_properties,
            cmd::sftp::read_remote_file_text,
            cmd::sftp::write_remote_file_text,
            cmd::sftp::create_remote_file,
            cmd::sftp::create_remote_dir,
            cmd::sftp::create_remote_symlink,
            cmd::sftp::chmod_remote_file,
            cmd::sftp::update_remote_file_attributes,
            cmd::sftp::download_remote_directory,
            cmd::sftp::upload_local_directory,
            cmd::sftp::pause_transfer,
            cmd::sftp::resume_transfer,
            cmd::sftp::cancel_transfer,
            cmd::sftp::respond_transfer_duplicate,
            cmd::connection::get_saved_connections,
            cmd::connection::save_connection,
            cmd::connection::delete_connection,
            cmd::connection::get_connection_password_value,
            cmd::connection::reorder_items,
            cmd::connection::get_ssh_keys,
            cmd::connection::get_ssh_key_passphrase,
            cmd::connection::save_ssh_key,
            cmd::connection::delete_ssh_key,
            cmd::connection::get_groups,
            cmd::connection::save_group,
            cmd::connection::delete_group,
            cmd::connection::clear_all_connections,
            cmd::connection::get_quick_commands,
            cmd::connection::save_quick_commands,
            cmd::connection::upsert_quick_command,
            cmd::connection::increment_quick_command_use_count,
            cmd::connection::import_quick_commands,
            cmd::connection::get_saved_passwords,
            cmd::connection::get_saved_password_value,
            cmd::connection::save_password,
            cmd::connection::delete_password,
            cmd::credential::get_saved_credentials,
            cmd::credential::get_saved_credential_password,
            cmd::credential::save_credential,
            cmd::credential::delete_credential,
            cmd::settings::get_app_settings,
            cmd::settings::save_app_settings,
            cmd::settings::import_keyword_highlight_rules,
            cmd::settings::save_app_ui_settings,
            cmd::settings::verify_master_password,
            cmd::watcher::start_file_watch,
            cmd::watcher::stop_file_watch,
            cmd::translate::translate_text,
            cmd::importer::import_sessions,
            cmd::backup::export_config,
            cmd::backup::import_config,
            cmd::stats::get_remote_stats,
            cmd::stats::get_terminal_cwd,
            cmd::tunnel::get_tunnels,
            cmd::tunnel::get_tunnel_groups,
            cmd::tunnel::save_tunnel,
            cmd::tunnel::save_tunnel_group,
            cmd::tunnel::set_tunnel_group,
            cmd::tunnel::delete_tunnel,
            cmd::tunnel::delete_tunnel_group,
            cmd::tunnel::open_tunnel,
            cmd::tunnel::close_tunnel,
            cmd::proxy::get_proxies,
            cmd::proxy::get_proxy_groups,
            cmd::proxy::save_proxy,
            cmd::proxy::save_proxy_group,
            cmd::proxy::set_proxy_group,
            cmd::proxy::delete_proxy,
            cmd::proxy::delete_proxy_group,
            cmd::proxy::get_proxy_password,
            cmd::otp::get_otp_entries,
            cmd::otp::get_otp_secret_value,
            cmd::otp::save_otp_entry,
            cmd::otp::delete_otp_entry,
            cmd::otp::generate_otp_code,
            cmd::otp::import_otp_from_qr,
        ])
        .build(context)
        .expect("error while building tauri application")
        .run(|_app, _event| {
            #[cfg(target_os = "macos")]
            if matches!(_event, tauri::RunEvent::Reopen { .. }) {
                app::show_main_window(_app);
            }
        });
}
