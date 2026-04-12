#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod error;
mod fs;
mod index;
mod search;
mod config;

#[cfg(desktop)]
mod tray;

use commands::search::AppState;
use index::monitor::VolumeManager;
use log::info;
use std::sync::Arc;
use tauri::{Emitter, Manager};
use tokio::sync::Mutex;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    info!("Starting Everything Tauri v{}", env!("CARGO_PKG_VERSION"));

    // 解析命令行参数
    let args: Vec<String> = std::env::args().collect();
    let silent_mode = args.contains(&"-s".to_string()) || args.contains(&"-S".to_string());
    
    if silent_mode {
        info!("Starting in silent mode");
    }

    let volume_manager = Arc::new(Mutex::new(VolumeManager::new()));

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            index_manager: index::IndexManager::new(std::path::Path::new("everything.db")).unwrap(),
            volume_manager: volume_manager.clone(),
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // 阻止窗口关闭
                api.prevent_close();
                // 隐藏窗口到系统托盘
                window.hide().unwrap();
                log::info!("Window minimized to system tray");
            }
        })
        .setup(move |app| {
            info!("Application setup started");

            let vm = volume_manager.clone();
            let handle = app.handle().clone();
            let handle_for_tray = app.handle().clone();
            
            // 如果不是静默模式，显示窗口
            if !silent_mode {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    log::info!("Main window shown in normal mode");
                }
            }
           
            tauri::async_runtime::spawn(async move {
                let is_admin = fs::is_elevated();
                let mut volume_manager = vm.lock().await;
                
                let config = crate::config::Config::load().ok();
                let monitored_from_config = config.as_ref().map(|c| c.monitored_volumes.clone()).unwrap_or_default();
                
                if !monitored_from_config.is_empty() {
                    for volume in &monitored_from_config {
                        if let Err(e) = volume_manager.add_volume(volume, is_admin) {
                            log::warn!("Failed to add volume {} from config: {}", volume, e);
                        }
                    }
                } else if is_admin {
                    if let Ok(volumes) = fs::get_ntfs_volumes() {
                        for volume in &volumes {
                            if let Err(e) = volume_manager.add_volume(&volume.drive_letter, is_admin) {
                                log::warn!("Failed to add volume {}: {}", volume.drive_letter, e);
                            }
                        }
                    }
                } else {
                    if let Err(e) = volume_manager.add_volume("D:", is_admin) {
                        log::warn!("Failed to add volume D:: {}", e);
                    }
                }

                for drive_letter in volume_manager.volumes() {
                    let mut monitor = volume_manager.take_monitor(&drive_letter);
                    if let Some(mut m) = monitor {
                        let handle_clone = handle.clone();
                        if let Ok(count) = m.scan_with_progress_callback(&handle_clone) {
                            log::info!("Scanned volume {}: {} files", drive_letter, count);
                            let _ = handle.emit("scan-complete", serde_json::json!({
                                "volume": drive_letter,
                                "count": count
                            }));
                        }
                        volume_manager.return_monitor(&drive_letter, m);
                    }
                }
                
                drop(volume_manager);

                if is_admin {
                    let vm_clone = vm.clone();
                    let handle_clone = handle.clone();
                    
                    tauri::async_runtime::spawn(async move {
                        loop {
                            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                            
                            let mut volume_manager = vm_clone.lock().await;
                            
                            for drive_letter in volume_manager.volumes() {
                                let mut monitor = volume_manager.take_monitor(&drive_letter);
                                if let Some(ref mut m) = monitor {
                                    if let Ok(count) = m.scan() {
                                        if count > 0 {
                                            log::info!("Incremental update for {}: {} new/changed files", drive_letter, count);
                                            let _ = handle_clone.emit("index-updated", serde_json::json!({
                                                "volume": drive_letter,
                                                "count": count
                                            }));
                                        }
                                    }
                                }
                                if let Some(m) = monitor {
                                    volume_manager.return_monitor(&drive_letter, m);
                                }
                            }
                        }
                    });
                }
            });

            #[cfg(desktop)]
            {
                if let Err(e) = tray::setup_tray(handle_for_tray) {
                    log::error!("Failed to setup tray: {}", e);
                }
            }

            info!("Application setup completed");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::search::search_files,
            commands::search::get_search_suggestions,
            commands::volume::get_volumes,
            commands::volume::add_volume,
            commands::volume::remove_volume,
            commands::volume::refresh_volumes,
            commands::volume::rebuild_index,
            commands::volume::optimize_index,
            commands::volume::get_index_status,
            commands::volume::get_monitored_volumes,
            commands::file::open_file,
            commands::file::open_folder,
            commands::file::delete_file,
            commands::file::copy_file,
            commands::file::move_file,
            commands::export::export_csv,
            commands::export::export_txt,
            commands::export::export_json,
            commands::export::export_all_results,
            commands::config::get_config,
            commands::config::save_config,
            commands::system::is_admin,
            commands::system::add_startup,
            commands::system::remove_startup,
            commands::system::is_startup_enabled,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}