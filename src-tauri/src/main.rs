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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{Emitter, Manager};
use tokio::sync::Mutex;

/**
 * 主函数，程序的入口点
 * 
 * 功能：
 * 1. 初始化日志系统
 * 2. 解析命令行参数，检查是否为静默模式
 * 3. 创建卷管理器
 * 4. 配置 Tauri 应用
 * 5. 设置窗口关闭事件处理
 * 6. 初始化应用设置
 * 7. 注册命令处理函数
 * 8. 运行应用
 */
fn main() {
    // 初始化日志系统，默认日志级别为 info
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    // 记录应用启动信息
    info!("Starting Everything Tauri v{}", env!("CARGO_PKG_VERSION"));

    // 解析命令行参数，检查是否包含 -s 或 -S 参数（静默启动）
    let args: Vec<String> = std::env::args().collect();
    let silent_mode = args.contains(&"-s".to_string()) || args.contains(&"-S".to_string());
    
    // 如果是静默模式，记录日志
    if silent_mode {
        info!("Starting in silent mode");
    }

    // 创建卷管理器，使用 Arc<Mutex> 实现线程安全
    let volume_manager = Arc::new(Mutex::new(VolumeManager::new()));
    let is_searching = Arc::new(AtomicBool::new(false));
    let last_index_update = Arc::new(Mutex::new(String::new()));

    // 构建 Tauri 应用
    tauri::Builder::default()
        // 注册 shell 插件
        .plugin(tauri_plugin_shell::init())
        // 注册 dialog 插件
        .plugin(tauri_plugin_dialog::init())
        // 管理应用状态
        .manage(AppState {
            // 创建索引管理器，使用 everything.db 作为数据库
            index_manager: index::IndexManager::new(std::path::Path::new("everything.db")).unwrap(),
            // 克隆卷管理器到应用状态
            volume_manager: volume_manager.clone(),
            is_searching: is_searching.clone(),
            last_index_update: last_index_update.clone(),
        })
        // 设置窗口事件处理
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                #[cfg(not(debug_assertions))]
                {
                    api.prevent_close();
                    window.hide().unwrap();
                    log::info!("Window minimized to system tray");
                }
                #[cfg(debug_assertions)]
                {
                    log::info!("Window close requested (debug mode: allowing close)");
                }
            }
        })
        // 设置应用初始化
        .setup(move |app| {
            // 记录应用设置开始
            info!("Application setup started");

            // 克隆卷管理器和应用句柄
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
           
            // 启动异步任务
            tauri::async_runtime::spawn(async move {
                // 检查是否以管理员身份运行
                let is_admin = fs::is_elevated();
                // 获取卷管理器的锁
                let mut volume_manager = vm.lock().await;
                
                // 加载配置
                let config = crate::config::Config::load().ok();
                // 获取配置中的监控卷
                let monitored_from_config = config.as_ref().map(|c| c.monitored_volumes.clone()).unwrap_or_default();
                // 获取配置中的索引设置
                let include_hidden_files = config.as_ref().map(|c| c.index_settings.include_hidden_files).unwrap_or(false);
                let include_system_files = config.as_ref().map(|c| c.index_settings.include_system_files).unwrap_or(false);
                // 获取配置中的扫描所有卷选项
                let scan_all_volumes = config.as_ref().map(|c| c.scan_all_volumes).unwrap_or(false);
                
                // 如果配置中启用了扫描所有卷，添加所有 NTFS 卷
                if scan_all_volumes && is_admin {
                    if let Ok(volumes) = fs::get_ntfs_volumes() {
                        for volume in &volumes {
                            if let Err(e) = volume_manager.add_volume(&volume.drive_letter, is_admin, include_hidden_files, include_system_files) {
                                log::warn!("Failed to add volume {}: {}", volume.drive_letter, e);
                            }
                        }
                    }
                } else if !monitored_from_config.is_empty() {
                    // 如果配置中有监控卷，添加这些卷
                    for volume in &monitored_from_config {
                        if let Err(e) = volume_manager.add_volume(volume, is_admin, include_hidden_files, include_system_files) {
                            log::warn!("Failed to add volume {} from config: {}", volume, e);
                        }
                    }
                } else if is_admin {
                    // 如果以管理员身份运行，添加所有 NTFS 卷
                    if let Ok(volumes) = fs::get_ntfs_volumes() {
                        for volume in &volumes {
                            if let Err(e) = volume_manager.add_volume(&volume.drive_letter, is_admin, include_hidden_files, include_system_files) {
                                log::warn!("Failed to add volume {}: {}", volume.drive_letter, e);
                            }
                        }
                    }
                } else {
                    // 如果不是管理员，默认添加 C 盘
                    if let Err(e) = volume_manager.add_volume("C:", is_admin, include_hidden_files, include_system_files) {
                        log::warn!("Failed to add volume C:: {}", e);
                    }
                }

                // 扫描所有卷
                for drive_letter in volume_manager.volumes() {
                    // 获取卷监控器
                    let mut monitor = volume_manager.take_monitor(&drive_letter);
                    if let Some(mut m) = monitor {
                        // 克隆应用句柄
                        let handle_clone = handle.clone();
                        // 扫描卷并显示进度
                        if let Ok(count) = m.scan_with_progress_callback(&handle_clone) {
                            // 记录扫描结果
                            log::info!("Scanned volume {}: {} files", drive_letter, count);
                            // 发送扫描完成事件
                            let _ = handle.emit("scan-complete", serde_json::json!({
                                "volume": drive_letter,
                                "count": count
                            }));
                        }
                        // 将监控器返回给卷管理器
                        volume_manager.return_monitor(&drive_letter, m);
                    }
                }
                
                // 释放卷管理器锁
                drop(volume_manager);

                // 如果以管理员身份运行，启动增量更新任务
                if is_admin {
                    // 克隆卷管理器和应用句柄
                    let vm_clone = vm.clone();
                    let handle_clone = handle.clone();
                    let is_searching_clone = is_searching.clone();
                    let last_update_clone = last_index_update.clone();

                    // 启动增量更新任务
                    tauri::async_runtime::spawn(async move {
                        // 无限循环，每120秒执行一次
                        loop {
                            tokio::time::sleep(tokio::time::Duration::from_secs(120)).await;

                            // 如果正在搜索，跳过增量更新
                            if is_searching_clone.load(Ordering::SeqCst) {
                                continue;
                            }

                            // 获取卷管理器的锁
                            let mut volume_manager = vm_clone.lock().await;

                            // 对每个卷执行增量扫描
                            for drive_letter in volume_manager.volumes() {
                                // 获取卷监控器
                                let mut monitor = volume_manager.take_monitor(&drive_letter);
                                if let Some(ref mut m) = monitor {
                                    // 执行增量扫描
                                    if let Ok(count) = m.scan() {
                                        // 如果有新文件或修改的文件，发送索引更新事件
                                        if count > 0 {
                                            log::info!("Incremental update for {}: {} new/changed files", drive_letter, count);
                                            let _ = handle_clone.emit("index-updated", serde_json::json!({
                                                "volume": drive_letter,
                                                "count": count
                                            }));
                                        }
                                    }
                                }
                                // 将监控器返回给卷管理器
                                if let Some(m) = monitor {
                                    volume_manager.return_monitor(&drive_letter, m);
                                }
                            }

                            // 更新最后索引更新时间
                            let now = chrono::Local::now().format("%H:%M:%S").to_string();
                            *last_update_clone.lock().await = now;
                        }
                    });
                }
            });

            // 桌面平台设置系统托盘
            #[cfg(desktop)]
            {
                if let Err(e) = tray::setup_tray(handle_for_tray) {
                    log::error!("Failed to setup tray: {}", e);
                }
            }

            // 记录应用设置完成
            info!("Application setup completed");
            Ok(())
        })
        // 注册命令处理函数
        .invoke_handler(tauri::generate_handler![
            // 搜索相关命令
            commands::search::search_files,
            commands::search::get_search_suggestions,
            commands::search::get_records_range,
            // 卷管理相关命令
            commands::volume::get_volumes,
            commands::volume::add_volume,
            commands::volume::remove_volume,
            commands::volume::refresh_volumes,
            commands::volume::rebuild_index,
            commands::volume::optimize_index,
            commands::volume::get_index_status,
            commands::volume::get_monitored_volumes,
            // 文件操作相关命令
            commands::file::open_file,
            commands::file::open_folder,
            commands::file::delete_file,
            commands::file::copy_file,
            commands::file::move_file,
            // 导出相关命令
            commands::export::export_csv,
            commands::export::export_txt,
            commands::export::export_json,
            commands::export::export_all_results,
            // 配置相关命令
            commands::config::get_config,
            commands::config::save_config,
            // 系统相关命令
            commands::system::is_admin,
            commands::system::add_startup,
            commands::system::remove_startup,
            commands::system::is_startup_enabled,
        ])
        // 运行应用
        .run(tauri::generate_context!())
        // 处理应用运行错误
        .expect("error while running tauri application");
}