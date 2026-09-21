mod archive;
mod archive_export;
mod qlogin;
mod qzone;

#[tauri::command]
fn exit_app(app: tauri::AppHandle) {
    app.exit(0);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mut context = tauri::generate_context!();
    let mut portable_cache: Option<std::path::PathBuf> = None;
    let mut portable_windows = Vec::new();
    #[cfg(windows)]
    if let Ok(executable) = std::env::current_exe() {
        if let Some(directory) = executable.parent() {
            if let Ok(text) = std::fs::read_to_string(directory.join("archive-storage.json")) {
                if let Ok(settings) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(cache) = settings
                        .get("webviewDataDirectory")
                        .and_then(|v| v.as_str())
                    {
                        let path = std::path::PathBuf::from(cache);
                        if path.is_absolute() {
                            portable_cache = Some(path);
                            for window in &mut context.config_mut().app.windows {
                                if window.create {
                                    portable_windows.push(window.clone());
                                    window.create = false;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    tauri::Builder::default()
        .setup(move |app| {
            for config in &portable_windows {
                let mut builder = tauri::WebviewWindowBuilder::from_config(app, config)?;
                if let Some(cache) = &portable_cache {
                    builder = builder.data_directory(cache.clone());
                }
                builder.build()?;
            }
            Ok(())
        })
        .manage(archive::ArchiveState::new())
        .manage(archive_export::ArchiveTransferState::new())
        .manage(qlogin::QLoginState::new())
        .manage(qzone::RecycleAuthState::default())
        .plugin(tauri_plugin_http::init())
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            exit_app,
            qlogin::start_qr_login,
            qlogin::poll_qr_login,
            qlogin::get_login_status,
            qlogin::logout_qzone,
            qlogin::open_web_login,
            qlogin::check_web_login,
            qlogin::sync_cookies_to_webview,
            qzone::fetch_first_feeds,
            qzone::fetch_more_feeds,
            qzone::open_recycle_password_window,
            qzone::check_recycle_password,
            qzone::close_recycle_password_window,
            qzone::list_recycle_albums,
            qzone::list_recycle_photos,
            qzone::load_recycle_photo_preview,
            qzone::list_qzone_albums,
            qzone::create_qzone_album,
            qzone::recover_recycle_album,
            qzone::recover_recycle_photos,
            archive::start_feed_archive,
            archive::start_legacy_history_scan,
            archive::recommend_legacy_max_offset,
            archive::get_archive_progress,
            archive::cancel_feed_archive,
            archive::list_archive_skips,
            archive::clear_resolved_archive_skips,
            archive::retry_all_archive_skips,
            archive::retry_archive_skip,
            archive::list_archived_feeds,
            archive::list_archived_media,
            archive::get_archived_feed,
            archive::count_archived_feeds,
            archive::export_archived_html,
            archive::load_archived_image,
            archive::load_archived_video,
            archive::get_archive_overview,
            archive::list_interactors,
            archive::get_interaction_ranking,
            archive::delete_archived_feeds,
            archive::clear_archived_feeds,
            archive::delete_all_app_data,
            archive::get_archive_storage_info,
            archive::set_archive_storage_dir,
            archive_export::start_media_download,
            archive_export::get_transfer_progress,
            archive_export::cancel_transfer,
            archive_export::export_archive_pdf,
            archive_export::export_share_zip,
        ])
        .run(context)
        .expect("error while running tauri application");
}
