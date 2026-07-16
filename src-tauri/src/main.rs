#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod config;
mod error;
mod file_logger; // 文件日志模块：将启动信息写入 %APPDATA%\Everything\logs\
mod fs;
mod index;
mod search;
mod tray_notification; // 托盘气泡通知模块：开机自启动时显示通知

#[cfg(desktop)]
mod tray;

use commands::search::AppState;
use index::monitor::VolumeManager;
use index::UsnIndexManager;
use index::usn_types::UsnResponse;
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
/// 检查单例实例，如果已有实例在运行则将其窗口置前并退出。
/// 使用 Windows 命名 Mutex 实现跨进程检测。
fn ensure_single_instance() -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::SetLastError;
        use windows::Win32::System::Threading::CreateMutexW;
        use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE};

        unsafe {
            SetLastError(windows::Win32::Foundation::WIN32_ERROR(0));
            let name = "Everything_Tauri_Single_Instance\0";
            let name_wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            let _ = CreateMutexW(None, false, windows::core::PCWSTR(name_wide.as_ptr()));

            // 通过 Error::from_win32() 检查是否已存在（ERROR_ALREADY_EXISTS = 183）
            if windows::core::Error::from_win32().code().0 & 0x0000FFFF == 183 {
                log::info!("Another instance detected, activating existing window");
                // 查找已有窗口并置前
                let title = "Everything - 极速文件搜索\0";
                let title_wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
                let hwnd = FindWindowW(
                    windows::core::PCWSTR(std::ptr::null()),
                    windows::core::PCWSTR(title_wide.as_ptr()),
                );
                if hwnd.0 != 0 {
                    ShowWindow(hwnd, SW_RESTORE);
                    SetForegroundWindow(hwnd);
                }
                return false;
            }
            // 第一个实例，创建的 mutex 句柄在进程退出时由 OS 自动清理
        }
    }
    true
}

fn main() {
    // 单例检查：确保只有一个实例在运行
    if !ensure_single_instance() {
        std::process::exit(0);
    }

    // 初始化文件日志（先于 env_logger，确保所有启动信息都能记录到文件）
    // 日志文件位置: %APPDATA%\Everything\logs\everything-YYYY-MM-DD.log
    file_logger::init();

    // 初始化日志系统，使用自定义 DualLogger 同时输出到 stderr 和文件
    // 这样开机启动（无控制台）时仍能在日志文件中查看
    log::set_logger(&file_logger::DualLogger)
        .map(|()| log::set_max_level(log::LevelFilter::Info))
        .ok();

    // 记录应用启动信息
    info!("=================================================");
    info!("Starting Everything Tauri v{}", env!("CARGO_PKG_VERSION"));
    info!("Log directory: {:?}", file_logger::log_dir_path());
    info!("=================================================");

    // 解析命令行参数，检查是否包含 -s 或 -S 参数（静默启动）
    let args: Vec<String> = std::env::args().collect();
    let silent_mode = args.contains(&"-s".to_string()) || args.contains(&"-S".to_string());

    // 获取程序自身路径（用于启动通知等）
    let exe_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
        .unwrap_or_default();
    info!("Executable path: {}", exe_path);
    info!("Launch arguments: {:?}", args);

    // 记录当前工作目录（从 Run 键启动时可能与预期不同）
    if let Ok(cwd) = std::env::current_dir() {
        info!("Current working directory: {:?}", cwd);
    }

    // 如果是静默模式，记录日志
    // 通知将在 Tauri setup 完成、托盘创建之后显示（见下方 setup 回调）
    if silent_mode {
        info!("Starting in silent mode (started with -s/-S flag)");
    }

    // 创建卷管理器，使用 Arc<Mutex> 实现线程安全
    let volume_manager = Arc::new(Mutex::new(VolumeManager::new()));
    let is_searching = Arc::new(AtomicBool::new(false));
    let last_index_update = Arc::new(Mutex::new(String::new()));

    // 检查管理员权限，决定是否使用 USN Journal
    let is_admin = fs::is_elevated();
    let usn_manager: Option<Arc<UsnIndexManager>> = if is_admin {
        info!("Admin mode detected, creating USN Index Manager");
        Some(Arc::new(UsnIndexManager::new()))
    } else {
        info!("Non-admin mode, using walkdir for indexing");
        None
    };

    // 数据库路径：使用 AppData 目录，避免在 dev 模式下触发文件监听器重建
    let db_path = dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("Everything");
    info!("Database directory: {:?}", db_path);
    match std::fs::create_dir_all(&db_path) {
        Ok(_) => info!("Database directory created/verified"),
        Err(e) => log::error!("Failed to create database directory: {}", e),
    }
    let db_path = db_path.join("everything.db");
    info!("Database path: {:?}", db_path);

    // 初始化索引管理器
    info!("Initializing index manager...");
    let index_manager = match index::IndexManager::new(&db_path) {
        Ok(m) => {
            info!("Index manager initialized successfully");
            m
        }
        Err(e) => {
            log::error!("FATAL: Failed to initialize index manager: {}", e);
            eprintln!("FATAL: Failed to initialize index manager: {}", e);
            std::process::exit(1);
        }
    };

    // 构建 Tauri 应用
    info!("Building Tauri application...");
    tauri::Builder::default()
        // 注册 shell 插件
        .plugin(tauri_plugin_shell::init())
        // 注册 dialog 插件
        .plugin(tauri_plugin_dialog::init())
        // 注册 notification 插件（用于开机启动时显示 Toast 通知）
        .plugin(tauri_plugin_notification::init())
        // 管理应用状态
        .manage(AppState {
            index_manager,
            // 克隆卷管理器到应用状态
            volume_manager: volume_manager.clone(),
            is_searching: is_searching.clone(),
            last_index_update: last_index_update.clone(),
            usn_manager: usn_manager.clone(),
        })
        // 设置窗口事件处理
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
                log::info!("Window hidden to tray");
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
                // 获取卷管理器的锁
                let mut volume_manager = vm.lock().await;

                // 加载配置
                let config = crate::config::Config::load().ok();
                // 获取配置中的监控卷
                let monitored_from_config = config
                    .as_ref()
                    .map(|c| c.monitored_volumes.clone())
                    .unwrap_or_default();
                // 获取配置中的索引设置
                let include_hidden_files = config
                    .as_ref()
                    .map(|c| c.index_settings.include_hidden_files)
                    .unwrap_or(false);
                let include_system_files = config
                    .as_ref()
                    .map(|c| c.index_settings.include_system_files)
                    .unwrap_or(false);
                // 获取配置中的扫描所有卷选项
                let scan_all_volumes = config.as_ref().map(|c| c.scan_all_volumes).unwrap_or(false);
                log::info!(
                    "Config: scan_all_volumes={}, admin={}, monitored={:?}",
                    scan_all_volumes, is_admin, monitored_from_config
                );

                if scan_all_volumes {
                    // 扫描所有卷（不依赖管理员权限，非管理员也能尝试添加可访问的卷）
                    log::info!("scan_all_volumes is enabled, adding all NTFS volumes");
                    if let Ok(volumes) = fs::get_ntfs_volumes() {
                        for volume in &volumes {
                            if let Err(e) = volume_manager.add_volume(
                                &volume.drive_letter,
                                is_admin,
                                include_hidden_files,
                                include_system_files,
                            ) {
                                log::warn!("Failed to add volume {}: {}", volume.drive_letter, e);
                            }
                        }
                    }
                } else if !monitored_from_config.is_empty() {
                    // 如果配置中有监控卷，添加这些卷
                    log::info!("Adding volumes from config: {:?}", monitored_from_config);
                    for volume in &monitored_from_config {
                        if let Err(e) = volume_manager.add_volume(
                            volume,
                            is_admin,
                            include_hidden_files,
                            include_system_files,
                        ) {
                            log::warn!("Failed to add volume {} from config: {}", volume, e);
                        }
                    }
                } else if is_admin {
                    // 如果以管理员身份运行，添加所有 NTFS 卷
                    log::info!("Admin mode: adding all NTFS volumes");
                    if let Ok(volumes) = fs::get_ntfs_volumes() {
                        for volume in &volumes {
                            if let Err(e) = volume_manager.add_volume(
                                &volume.drive_letter,
                                is_admin,
                                include_hidden_files,
                                include_system_files,
                            ) {
                                log::warn!("Failed to add volume {}: {}", volume.drive_letter, e);
                            }
                        }
                    }
                } else {
                    // 如果不是管理员，默认添加 C 盘
                    log::info!("Non-admin mode: adding default C: volume");
                    if let Err(e) = volume_manager.add_volume(
                        "C:",
                        is_admin,
                        include_hidden_files,
                        include_system_files,
                    ) {
                        log::warn!("Failed to add volume C:: {}", e);
                    }
                }

                // If admin, mark all monitors for USN and skip walkdir full scan
                if is_admin {
                    for drive_letter in volume_manager.volumes() {
                        if let Some(monitor) = volume_manager.get_monitor_mut(&drive_letter) {
                            monitor.use_usn = true;
                        }
                    }
                }

                // 扫描所有卷 (walkdir for non-USN volumes only)
                for drive_letter in volume_manager.volumes() {
                    // 获取卷监控器
                    let monitor = volume_manager.take_monitor(&drive_letter);
                    if let Some(mut m) = monitor {
                        if m.use_usn {
                            // USN volumes are scanned via the USN worker
                            volume_manager.return_monitor(&drive_letter, m);
                            continue;
                        }
                        // 克隆应用句柄
                        let handle_clone = handle.clone();
                        // 扫描卷并显示进度
                        if let Ok(count) = m.scan_with_progress_callback(&handle_clone) {
                            // 记录扫描结果
                            log::info!("Scanned volume {}: {} files", drive_letter, count);
                            // 发送扫描完成事件
                            let _ = handle.emit(
                                "scan-complete",
                                serde_json::json!({
                                    "volume": drive_letter,
                                    "count": count
                                }),
                            );
                        }
                        // 将监控器返回给卷管理器
                        volume_manager.return_monitor(&drive_letter, m);
                    }
                }

                // If admin, issue full-scan commands to the USN worker
                if let Some(ref usn) = usn_manager {
                    for drive_letter in volume_manager.volumes() {
                        let dl_char = drive_letter.chars().next().unwrap_or('C');
                        log::info!("[USN] Issuing full scan for drive {} (hidden={}, system={})", dl_char, include_hidden_files, include_system_files);
                        usn.full_scan(dl_char, include_hidden_files, include_system_files);
                    }
                }

                // 释放卷管理器锁
                drop(volume_manager);

                // Channel to signal when first full scan result arrives
                let (full_scan_done_tx, full_scan_done_rx) = tokio::sync::watch::channel(false);

                // Spawn USN response handler thread (admin only)
                if let Some(ref usn) = usn_manager {
                    let resp_rx = usn.resp_rx_clone();
                    let vm_for_handler = vm.clone();
                    let handle_for_handler = handle.clone();
                    let rt_handle = tokio::runtime::Handle::current();
                    let scan_done_tx = full_scan_done_tx;

                    std::thread::Builder::new()
                        .name("usn-response-handler".into())
                        .spawn(move || {
                            log::info!("[USN] Response handler thread started");
                            loop {
                                match resp_rx.recv() {
                                    Ok(UsnResponse::FullScanResult {
                                        drive_letter,
                                        files,
                                        path_table,
                                        ..
                                    }) => {
                                        let drive_string = format!("{}:", drive_letter);
                                        log::info!(
                                            "[USN] Full scan result for {}: {} files, {} paths",
                                            drive_string,
                                            files.len(),
                                            path_table.len()
                                        );
                                        let mut vm = rt_handle.block_on(vm_for_handler.lock());
                                        vm.apply_full_scan(&drive_string, files, path_table);
                                        let count = vm.get_file_count(&drive_string);
                                        drop(vm);
                                        let _ = handle_for_handler.emit(
                                            "scan-complete",
                                            serde_json::json!({
                                                "volume": drive_string,
                                                "count": count
                                            }),
                                        );
                                        // Signal polling task that full scan data is available
                                        let _ = scan_done_tx.send(true);
                                    }
                                    Ok(UsnResponse::IncrementalResult {
                                        drive_letter,
                                        added,
                                        removed,
                                        updated,
                                        ..
                                    }) => {
                                        if added.is_empty()
                                            && removed.is_empty()
                                            && updated.is_empty()
                                        {
                                            continue;
                                        }
                                        let drive_string = format!("{}:", drive_letter);
                                        log::info!(
                                            "[USN] Incremental {}: +{} ~{} -{}",
                                            drive_string,
                                            added.len(),
                                            updated.len(),
                                            removed.len()
                                        );
                                        let mut vm = rt_handle.block_on(vm_for_handler.lock());
                                        let total = vm.get_file_count(&drive_string);
                                        vm.apply_incremental_usn(
                                            &drive_string, added, removed, updated,
                                        );
                                        let new_total = vm.get_file_count(&drive_string);
                                        drop(vm);
                                        let _ = handle_for_handler.emit(
                                            "index-updated",
                                            serde_json::json!({
                                                "volume": drive_string,
                                                "added": new_total.saturating_sub(total),
                                                "updated": 0,
                                                "removed": total.saturating_sub(new_total),
                                                "total": new_total,
                                                "cache_total": new_total
                                            }),
                                        );
                                    }
                                    Ok(UsnResponse::Error { message }) => {
                                        log::error!("[USN] Worker error: {}", message);
                                    }
                                    Err(_) => {
                                        log::info!(
                                            "[USN] Response channel closed, handler exiting"
                                        );
                                        break;
                                    }
                                }
                            }
                        })
                        .expect("failed to spawn USN response handler thread");
                }

                // Spawn USN polling task (admin only)
                if let Some(ref usn) = usn_manager {
                    let usn_clone = usn.clone();
                    let vm_clone_for_usn = vm.clone();
                    let mut scan_done_rx = full_scan_done_rx;

                    tauri::async_runtime::spawn(async move {
                        // 等待全量扫描完成后再开始轮询
                        if !*scan_done_rx.borrow() {
                            log::info!("[USN] Polling task waiting for full scan to complete...");
                            while scan_done_rx.changed().await.is_ok() {
                                if *scan_done_rx.borrow() {
                                    break;
                                }
                            }
                        }
                        log::info!("[USN] Polling task started");

                        loop {
                            let config = crate::config::Config::load().ok();
                            let interval = config
                                .as_ref()
                                .map(|c| c.index_settings.update_interval as u64)
                                .unwrap_or(30);
                            let include_hidden = config
                                .as_ref()
                                .map(|c| c.index_settings.include_hidden_files)
                                .unwrap_or(false);
                            let include_system = config
                                .as_ref()
                                .map(|c| c.index_settings.include_system_files)
                                .unwrap_or(false);

                            if interval == 0 {
                                log::info!("[USN] Poll interval is 0, skipping");
                                tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                                continue;
                            }

                            let volumes_list = {
                                let vm = vm_clone_for_usn.lock().await;
                                vm.volumes()
                            };

                            log::info!(
                                "[USN] Polling {} volumes: {:?}, interval={}s",
                                volumes_list.len(),
                                volumes_list,
                                interval
                            );
                            for drive_letter in &volumes_list {
                                let dl_char =
                                    drive_letter.chars().next().unwrap_or('C');
                                log::info!("[USN] Sending PollChanges for drive {}", dl_char);
                                usn_clone.poll_changes(dl_char, include_hidden, include_system);
                            }

                            tokio::time::sleep(tokio::time::Duration::from_secs(interval))
                                .await;
                        }
                    });
                }

                // 启动增量更新任务（所有用户）
                let vm_clone = vm.clone();
                let handle_clone = handle.clone();
                let is_searching_clone = is_searching.clone();
                let last_update_clone = last_index_update.clone();

                tauri::async_runtime::spawn(async move {
                    let mut next_scan: Option<tokio::time::Instant> = None;

                    // 启动后延迟 60 秒再开始增量更新，让前端先完成初始加载和排序
                    tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;

                    loop {
                        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

                        let config = crate::config::Config::load().ok();
                        let interval = config
                            .as_ref()
                            .map(|c| c.index_settings.update_interval as u64)
                            .unwrap_or(300);

                        if interval == 0 {
                            next_scan = None;
                            continue;
                        }

                        let now = tokio::time::Instant::now();
                        let should_scan = match next_scan {
                            Some(t) => now >= t,
                            None => true,
                        };
                        if !should_scan {
                            continue;
                        }

                        if is_searching_clone.load(Ordering::SeqCst) {
                            continue;
                        }

                        let include_hidden = config
                            .as_ref()
                            .map(|c| c.index_settings.include_hidden_files)
                            .unwrap_or(false);
                        let include_system = config
                            .as_ref()
                            .map(|c| c.index_settings.include_system_files)
                            .unwrap_or(false);

                        // 先获取卷列表（短暂持锁）
                        let volumes_list = {
                            let vm = vm_clone.lock().await;
                            vm.volumes()
                        };

                        for drive_letter in volumes_list {
                            // 如果有搜索请求，跳过剩余卷的扫描
                            if is_searching_clone.load(Ordering::SeqCst) {
                                log::info!("Incremental scan skipped: search in progress");
                                break;
                            }

                            // 扫描阶段：短暂持锁
                            let inc_result = {
                                let mut vm = vm_clone.lock().await;
                                let mut monitor = vm.take_monitor(&drive_letter);
                                let mut result = None;
                                if let Some(ref mut m) = monitor {
                                    if m.use_usn {
                                        // USN volumes are handled by the USN polling task
                                        if let Some(m) = monitor {
                                            vm.return_monitor(&drive_letter, m);
                                        }
                                        continue;
                                    }
                                    m.update_settings(include_hidden, include_system);
                                    if let Ok(r) = m.scan_incremental(&handle_clone) {
                                        if r.added > 0 || r.updated > 0 || r.removed > 0 {
                                            log::info!(
                                                "Incremental {}: +{} ~{} -{} (total: {})",
                                                drive_letter, r.added, r.updated, r.removed, r.total
                                            );
                                            result = Some(r);
                                        }
                                    }
                                }
                                if let Some(m) = monitor {
                                    vm.return_monitor(&drive_letter, m);
                                }
                                result
                            };

                            // 更新缓存阶段：短暂持锁
                            if let Some(result) = inc_result {
                                let cache_total = {
                                    let mut vm = vm_clone.lock().await;
                                    vm.apply_incremental(&drive_letter, &result)
                                };
                                let _ = handle_clone.emit(
                                    "index-updated",
                                    serde_json::json!({
                                        "volume": drive_letter,
                                        "added": result.added,
                                        "updated": result.updated,
                                        "removed": result.removed,
                                        "total": result.total,
                                        "cache_total": cache_total
                                    }),
                                );
                            }
                            // 锁已释放，前端请求可以继续
                        }

                        let now_str = chrono::Local::now().format("%H:%M:%S").to_string();
                        *last_update_clone.lock().await = now_str;
                        next_scan = Some(tokio::time::Instant::now() + tokio::time::Duration::from_secs(interval));
                    }
                });
            });

            // 桌面平台设置系统托盘
            #[cfg(desktop)]
            {
                if let Err(e) = tray::setup_tray(handle_for_tray.clone()) {
                    log::error!("Failed to setup tray: {}", e);
                }
            }

            // 静默启动时显示 Toast 通知（延迟3秒，确保托盘和系统UI就绪）
            if silent_mode {
                let app_handle_for_notify = handle_for_tray.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    log::info!("显示开机启动通知...");
                    tray_notification::show_startup_notification(&app_handle_for_notify);
                });
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
            commands::search::get_sorted_range,
            // 卷管理相关命令
            commands::volume::get_volumes,
            commands::volume::add_volume,
            commands::volume::remove_volume,
            commands::volume::refresh_volumes,
            commands::volume::rebuild_index,
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
            // 图标相关命令
            commands::icon::get_file_icon,
        ])
        // 运行应用
        .run(tauri::generate_context!())
        // 处理应用运行错误
        .unwrap_or_else(|e| {
            log::error!("Tauri application exited with error: {}", e);
            eprintln!("Tauri application exited with error: {}", e);
            std::process::exit(1);
        });

    info!("Application exited normally");
}
