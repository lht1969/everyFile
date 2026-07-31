#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod config;
mod error;
mod file_logger; // 文件日志模块：将启动信息写入 %APPDATA%\everyFile\logs\
mod fs;
mod index;
mod search;
mod tray_notification; // 托盘气泡通知模块：开机自启动时显示通知

#[cfg(desktop)]
mod tray;

use commands::search::AppState;
use index::monitor::VolumeManager;
use index::usn_types::UsnResponse;
use index::UsnIndexManager;
use log::info;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{Emitter, Manager};
use tokio::sync::Mutex;

/*
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
///
/// 使用 Windows 命名 Mutex 实现跨进程检测。
///
/// `takeover` 为 true（--elevated 提权重启的实例）时：旧实例（普通权限）在
/// `relaunch_elevated()` 成功后立即退出，此处轮询等待其释放 Mutex 后接管成为
/// 唯一实例，避免新实例刚启动就撞见旧实例的 Mutex 而误退出。超时则退化为
/// "激活已有窗口并退出"。
fn ensure_single_instance(takeover: bool) -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{CloseHandle, SetLastError, WIN32_ERROR};
        use windows::Win32::System::Threading::CreateMutexW;
        use windows::Win32::UI::WindowsAndMessaging::{
            FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE,
        };

        unsafe {
            let name = "everyFile_Tauri_Single_Instance\0";
            let name_wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut acquired = false;
            loop {
                SetLastError(WIN32_ERROR(0));
                let handle = CreateMutexW(None, false, windows::core::PCWSTR(name_wide.as_ptr()));
                // 通过 Error::from_win32() 检查是否已存在（ERROR_ALREADY_EXISTS = 183）
                let already_exists =
                    windows::core::Error::from_win32().code().0 & 0x0000FFFF == 183;
                if already_exists {
                    // 关闭本进程对该已有 mutex 的重复句柄，避免句柄泄漏
                    if let Ok(h) = handle {
                        let _ = CloseHandle(h);
                    }
                    if !takeover || std::time::Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    continue;
                }
                // 创建成功：windows 0.52 的 HANDLE 不会自动关闭（无 Drop 实现），
                // 不调用 CloseHandle 即可让内核 mutex 持续占用，进程退出时由 OS 回收。
                let _ = handle;
                acquired = true;
                break;
            }

            if !acquired {
                log::info!("Another instance detected, activating existing window");
                // 查找已有窗口并置前
                let title = "everyFile - 极速文件搜索\0";
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
            // 第一个实例（或接管成功），mutex 句柄在进程退出时由 OS 自动清理
        }
    }
    true
}

fn main() {
    // 解析命令行参数（须先于单实例检查：--elevated 决定接管行为）
    let args: Vec<String> = std::env::args().collect();
    let silent_mode = args.contains(&"-s".to_string()) || args.contains(&"-S".to_string());
    let elevated_relaunch = args.iter().any(|a| a == "--elevated");

    // 单例检查：确保只有一个实例在运行
    if !ensure_single_instance(elevated_relaunch) {
        std::process::exit(0);
    }

    // 初始化文件日志（先于 env_logger，确保所有启动信息都能记录到文件）
    // 日志文件位置: %APPDATA%\everyFile\logs\everyFile-YYYY-MM-DD.log
    file_logger::init();

    // 初始化日志系统，使用自定义 DualLogger 同时输出到 stderr 和文件
    // 这样开机启动（无控制台）时仍能在日志文件中查看
    log::set_logger(&file_logger::DualLogger)
        .map(|()| log::set_max_level(log::LevelFilter::Info))
        .ok();

    // 记录应用启动信息
    info!("=================================================");
    info!("Starting everyFile v{}", env!("CARGO_PKG_VERSION"));
    info!("Log directory: {:?}", file_logger::log_dir_path());
    info!("=================================================");

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
    let scanning_volumes: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    // 检查管理员权限，决定是否使用 USN Journal
    let is_admin = fs::is_elevated();

    // 普通用户且开启了"快速增量更新（USN Journal）"开关时，尝试 UAC 提权重启。
    // 提权成功后旧实例立即退出，新实例以管理员令牌运行并自动走 USN 增量链路；
    // 用户取消/非管理员组时保持普通模式（walkdir），由 --elevated 标志防止递归提权。
    if !is_admin && !silent_mode && !elevated_relaunch {
        if let Ok(config) = crate::config::Config::load() {
            if config.index_settings.enable_usn_journal {
                log::info!("USN journal enabled but not elevated, requesting elevation...");
                match crate::fs::relaunch_elevated() {
                    Ok(()) => {
                        log::info!("Elevation accepted, exiting current instance");
                        std::process::exit(0);
                    }
                    Err(code) => {
                        log::warn!(
                            "Elevation request failed (code={}), continuing as normal user",
                            code
                        );
                    }
                }
            }
        }
    }

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
            // 克隆卷管理器到应用状态
            volume_manager: volume_manager.clone(),
            is_searching: is_searching.clone(),
            last_index_update: last_index_update.clone(),
            // 先用空占位，setup 阶段会把真实 UsnIndexManager 写入其中
            usn_manager: Arc::new(Mutex::new(None)),
            scanning_volumes: scanning_volumes.clone(),
        })
        // 设置窗口事件处理
        .on_window_event({
            let vm = volume_manager.clone();
            move |window, event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                    log::info!("Window hidden to tray");
                    // 释放后台可重建的内存（search_cache、sorted_indices、fid_index）
                    if let Ok(mut vm) = vm.try_lock() {
                        vm.release_idle_memory();
                    }
                }
            }
        })
        // 设置应用初始化
        .setup(move |app| {
            // 记录应用设置开始
            info!("Application setup started");

            // 克隆卷管理器和应用句柄
            let vm = volume_manager.clone();
            let sv = scanning_volumes.clone();
            let handle = app.handle().clone();
            let handle_for_tray = app.handle().clone();
            // 获取 AppState 中的 usn_manager 占位，setup 阶段完成后写入真实实例
            let usn_manager_state = app.state::<AppState>().usn_manager.clone();

            // 如果不是静默模式，显示窗口
            if !silent_mode {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    log::info!("Main window shown in normal mode");
                }
            }

            // 启动异步任务
            tauri::async_runtime::spawn(async move {
                // === 阶段 1：加载配置 + 添加卷 + 设置标志（短暂持锁）===
                let (volumes_to_scan, include_hidden_files, include_system_files) = {
                    let mut volume_manager = vm.lock().await;

                    let config = crate::config::Config::load().ok();
                    let monitored_from_config = config
                        .as_ref()
                        .map(|c| c.monitored_volumes.clone())
                        .unwrap_or_default();
                    let include_hidden_files = config
                        .as_ref()
                        .map(|c| c.index_settings.include_hidden_files)
                        .unwrap_or(false);
                    let include_system_files = config
                        .as_ref()
                        .map(|c| c.index_settings.include_system_files)
                        .unwrap_or(false);
                    log::info!(
                        "Config: admin={}, monitored={:?}",
                        is_admin,
                        monitored_from_config
                    );

                    if !monitored_from_config.is_empty() {
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
                            // 查询并设置文件系统类型
                            if let Ok(info) = fs::get_volume_info(volume) {
                                if let Some(monitor) = volume_manager.get_monitor_mut(volume) {
                                    monitor.file_system = info.file_system;
                                }
                            }
                        }
                    } else if is_admin {
                        log::info!("Admin mode: adding all volumes");
                        if let Ok(volumes) = fs::get_all_volumes() {
                            for volume in &volumes {
                                let is_ntfs = volume.file_system.eq_ignore_ascii_case("NTFS");
                                if let Err(e) = volume_manager.add_volume(
                                    &volume.drive_letter,
                                    is_admin && is_ntfs,  // 仅 NTFS 启用 USN
                                    include_hidden_files,
                                    include_system_files,
                                ) {
                                    log::warn!(
                                        "Failed to add volume {}: {}",
                                        volume.drive_letter,
                                        e
                                    );
                                }
                                // 设置文件系统类型
                                if let Some(monitor) = volume_manager.get_monitor_mut(&volume.drive_letter) {
                                    monitor.file_system = volume.file_system.clone();
                                }
                                log::info!("Volume {} ({})", volume.drive_letter, volume.file_system);
                            }
                        }
                    } else {
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

                    // If admin, mark NTFS monitors for USN (non-NTFS uses walkdir)
                    if is_admin {
                        for drive_letter in volume_manager.volumes() {
                            if let Some(monitor) = volume_manager.get_monitor_mut(&drive_letter) {
                                if monitor.file_system.eq_ignore_ascii_case("NTFS") {
                                    monitor.use_usn = true;
                                }
                            }
                        }
                    }

                    // 获取卷列表快照，用于后续逐卷扫描
                    let volumes_to_scan = volume_manager.volumes();
                    (volumes_to_scan, include_hidden_files, include_system_files)
                }; // lock released here

                let mut walkdir_monitors: Vec<(String, crate::index::monitor::VolumeMonitor)> = Vec::new();

                // === 阶段 2：并行扫描所有 walkdir 卷 ===
                // 一次性 take 所有 monitor，并发扫描，一次性 return，
                // 减少 volume_manager 持锁次数，不同磁盘的卷并行 I/O 加速扫描。
                {
                    let mut vm = vm.lock().await;
                    for drive_letter in &volumes_to_scan {
                        if let Some(m) = vm.take_monitor(drive_letter) {
                            if m.use_usn {
                                // USN 卷跳过 walkdir，直接 return
                                vm.return_monitor(drive_letter, m);
                            } else {
                                walkdir_monitors.push((drive_letter.clone(), m));
                            }
                        }
                    }
                } // lock released

                // 并行扫描所有 walkdir 卷
                // 标记 walkdir 卷为扫描中
                {
                    let walkdir_vols: Vec<String> = walkdir_monitors.iter().map(|(dl, _)| dl.clone()).collect();
                    let mut scan_vols = sv.lock().await;
                    *scan_vols = walkdir_vols;
                }
                let mut scan_futs = Vec::with_capacity(walkdir_monitors.len());
                for (drive_letter, mut monitor) in walkdir_monitors {
                    let handle_clone = handle.clone();
                    let dl = drive_letter.clone();
                    scan_futs.push(tokio::task::spawn_blocking(move || {
                        let result = monitor.scan_with_progress_callback(&handle_clone);
                        (dl, monitor, result)
                    }));
                }

                // 等待所有扫描完成，逐个 return monitor 并发送事件
                for fut in scan_futs {
                    if let Ok((drive_letter, monitor, scan_result)) = fut.await {
                        {
                            let mut vm = vm.lock().await;
                            vm.return_monitor(&drive_letter, monitor);
                            // 失效搜索缓存：新卷数据已就绪，避免 loadAllFiles() 复用旧缓存
                            vm.invalidate_search_cache();
                        }
                        // 从扫描中卷列表移除
                        {
                            let mut scan_vols = sv.lock().await;
                            scan_vols.retain(|v| v != &drive_letter);
                        }
                        match scan_result {
                            Ok(count) => {
                                log::info!("Scanned volume {}: {} files", drive_letter, count);
                                let _ = handle.emit(
                                    "scan-complete",
                                    serde_json::json!({
                                        "volume": drive_letter,
                                        "count": count
                                    }),
                                );
                            }
                            Err(e) => {
                                log::warn!("Failed to scan volume {}: {}", drive_letter, e);
                            }
                        }
                    }
                }

                // === 阶段 3：创建 per-volume workers 并 dispatch USN full scan ===
                // 创建 USN Index Manager（admin 模式下，为每个 NTFS 卷创建独立 worker）
                let pending_usn_scans = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let usn_manager: Option<Arc<UsnIndexManager>> = if is_admin {
                    info!("[USN] Creating USN Index Manager with per-volume workers");
                    let mut manager = UsnIndexManager::new();
                    let volumes = {
                        let vm = vm.lock().await;
                        vm.volumes()
                    };
                    // 只对 NTFS 卷创建 worker，非 NTFS 卷走 walkdir
                    let mut ntfs_vols = Vec::new();
                    for drive_letter in &volumes {
                        let is_ntfs = {
                            let vm = vm.lock().await;
                            vm.get_monitor(drive_letter)
                                .map(|m| m.file_system.eq_ignore_ascii_case("NTFS"))
                                .unwrap_or(false)
                        };
                        if is_ntfs {
                            ntfs_vols.push(drive_letter.clone());
                        } else {
                            log::info!("Volume {} is not NTFS, skipping USN scan", drive_letter);
                        }
                    }
                    // Create a dedicated worker for each NTFS volume
                    for dl in &ntfs_vols {
                        let dl_char = dl.chars().next().unwrap();
                        manager.add_volume(dl_char);
                        log::info!("[USN] Created worker for drive {}", dl_char);
                    }
                    let usn_arc = Arc::new(manager);
                    // 标记 NTFS 卷为扫描中（非 NTFS 卷已在 walkdir 阶段扫描完成）
                    {
                        let mut scan_vols = sv.lock().await;
                        *scan_vols = ntfs_vols.clone();
                    }
                    // Dispatch parallel scan commands to all workers
                    for drive_letter in &ntfs_vols {
                        let dl_char = drive_letter.chars().next().unwrap_or('C');
                        log::debug!(
                            "[USN] Dispatching parallel scan for drive {} (hidden={}, system={})",
                            dl_char,
                            include_hidden_files,
                            include_system_files
                        );
                        let _ = handle.emit(
                            "scan-progress",
                            serde_json::json!({"volume": drive_letter}),
                        );
                        usn_arc.full_scan(dl_char, include_hidden_files, include_system_files);
                    }
                    pending_usn_scans.store(ntfs_vols.len(), std::sync::atomic::Ordering::SeqCst);
                    // 如果没有 NTFS 卷，直接通知前端
                    if ntfs_vols.is_empty() {
                        let _ = handle.emit("scan-all-complete", ());
                    }
                    Some(usn_arc)
                } else {
                    // 非管理员模式：无 USN 扫描，walkdir 扫描已完成，直接通知前端
                    let _ = handle.emit("scan-all-complete", ());
                    None
                };

                // 将创建好的 UsnIndexManager（或 None）保存到 AppState，
                // 使设置界面添加卷、搜索后触发轮询等命令能正确使用 USN。
                *usn_manager_state.lock().await = usn_manager.clone();

                // 创建全量扫描完成信号：walkdir 扫描完成后立即发送，
                // USN full scan 结果到达时也会发送（双保险）
                let (full_scan_done_tx, full_scan_done_rx) = tokio::sync::watch::channel(false);
                // walkdir 全量扫描已在阶段 2 完成，直接发送完成信号
                let _ = full_scan_done_tx.send(true);

                // Spawn USN response handler thread (admin only)
                if let Some(ref usn) = usn_manager {
                    let resp_rx = usn.resp_rx_clone();
                    let vm_for_handler = vm.clone();
                    let handle_for_handler = handle.clone();
                    let sv_for_handler = sv.clone();
                    let rt_handle = tokio::runtime::Handle::current();
                    let scan_done_tx = full_scan_done_tx;
                    let pending_usn_scans = pending_usn_scans.clone();

                    std::thread::Builder::new()
                        .name("usn-response-handler".into())
                        .spawn(move || {
                            log::debug!("[USN] Response handler thread started");
                            loop {
                                match resp_rx.recv() {
                                    Ok(UsnResponse::FullScanResult {
                                        drive_letter,
                                        files,
                                        path_table,
                                        ..
                                    }) => {
                                        let drive_string = format!("{}:", drive_letter);
                                        log::debug!(
                                            "[USN] Full scan result for {}: {} files, {} paths",
                                            drive_string,
                                            files.len(),
                                            path_table.len()
                                        );
                                        let mut vm = rt_handle.block_on(vm_for_handler.lock());
                                        vm.apply_full_scan(&drive_string, files, path_table);
                                        let count = vm.get_file_count(&drive_string);
                                        drop(vm);
                                        // 从扫描中卷列表移除该卷
                                        {
                                            let mut scan_vols = rt_handle.block_on(sv_for_handler.lock());
                                            scan_vols.retain(|v| v != &drive_string);
                                        }
                                        let _ = handle_for_handler.emit(
                                            "scan-complete",
                                            serde_json::json!({
                                                "volume": drive_string,
                                                "count": count
                                            }),
                                        );
                                        let _ = handle_for_handler.emit(
                                            "index-updated",
                                            serde_json::json!({
                                                "volume": drive_string,
                                                "count": count
                                            }),
                                        );
                                        // Signal polling task that full scan data is available
                                        let _ = scan_done_tx.send(true);
                                        // 所有 USN 卷扫描完成后通知前端
                                        let remaining = pending_usn_scans.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                                        if remaining == 1 {
                                            log::info!("[USN] All volumes scanned, emitting scan-all-complete");
                                            let _ = handle_for_handler.emit("scan-all-complete", ());
                                        }
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
                                        log::debug!(
                                            "[USN] Incremental {}: +{} ~{} -{} (response received)",
                                            drive_string,
                                            added.len(),
                                            updated.len(),
                                            removed.len()
                                        );
                                        // 在 move 前记录变化数量和文件 ID，用于 records-refresh payload
                                        let refresh_added = added.len();
                                        let refresh_updated = updated.len();
                                        let refresh_removed = removed.len();
                                        // 收集变更文件的 file_id，供前端判断是否在可见范围内
                                        let changed_fids: Vec<u32> = added.iter()
                                            .map(|r| r.file_id)
                                            .chain(removed.iter().map(|(fid, _)| *fid as u32))
                                            .chain(updated.iter().map(|(fid, _)| *fid as u32))
                                            .collect();

                                        let t_handler = std::time::Instant::now();
                                        let mut vm = rt_handle.block_on(vm_for_handler.lock());
                                        let total = vm.get_file_count(&drive_string);
                                        vm.apply_incremental_usn(
                                            &drive_string,
                                            added,
                                            removed,
                                            updated,
                                        );
                                        let new_total = vm.get_file_count(&drive_string);
                                        // 不在 records-refresh 中发送 total：current_search_total() 是 O(n) 扫描，
                                        // 对 200 万文件耗时显著。前端保持旧 totalCount 即可，
                                        // 少量文件增减对滚动条高度的影响不可见。
                                        drop(vm);
                                        log::debug!(
                                            "[USN] Incremental {}: applied in {:?}, total {}→{}",
                                            drive_string,
                                            t_handler.elapsed(),
                                            total,
                                            new_total
                                        );
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
                                        let _ = handle_for_handler.emit(
                                            "records-refresh",
                                            serde_json::json!({
                                                "added": refresh_added,
                                                "updated": refresh_updated,
                                                "removed": refresh_removed,
                                                "changed_fids": changed_fids,
                                            }),
                                        );
                                    }
                                    Ok(UsnResponse::Error { drive_letter: _, message }) => {
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

                // 克隆 full_scan_done_rx 供 walkdir 增量任务使用（USN 轮询任务会 take 原始值）
                let incremental_done_rx = full_scan_done_rx.clone();

                // Spawn USN polling task (admin only)
                if let Some(ref usn) = usn_manager {
                    let usn_clone = usn.clone();
                    let vm_clone_for_usn = vm.clone();
                    let is_searching_for_usn = is_searching.clone();

                    tauri::async_runtime::spawn(async move {
                        // 等待所有卷的全量扫描完成（每个卷都有文件数据）再开始轮询
                        // 不依赖 watch channel 信号，因为 USN full scan 是异步的，
                        // 可能 C 盘完成后 D/E 盘还在扫描中
                        log::debug!("[USN] Polling task waiting for all volumes to have data...");
                        loop {
                            let all_ready = {
                                let vm = vm_clone_for_usn.lock().await;
                                let volumes = vm.volumes();
                                volumes.iter().all(|v| vm.get_file_count(v) > 0)
                            };
                            if all_ready {
                                break;
                            }
                            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                        }
                        // 额外等 3 秒让前端完成初始加载
                        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                        log::debug!("[USN] Polling task started");

                        // Read config once at startup; reload only when file changes
                        let mut config = crate::config::Config::load().ok();
                        let mut last_config_modified = crate::config::Config::config_path()
                            .ok()
                            .and_then(|p| std::fs::metadata(&p).ok())
                            .and_then(|m| m.modified().ok());

                        loop {
                            // Reload config only if file modification time changed
                            let config_path = crate::config::Config::config_path().ok();
                            if let Some(ref path) = config_path {
                                if let Ok(meta) = std::fs::metadata(path) {
                                    if let Ok(modified) = meta.modified() {
                                        if last_config_modified.as_ref() != Some(&modified) {
                                            config = crate::config::Config::load().ok();
                                            last_config_modified = Some(modified);
                                        }
                                    }
                                }
                            }

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
                                log::debug!("[USN] Poll interval is 0, skipping");
                                tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                                continue;
                            }

                            // 搜索进行中时跳过轮询，避免与搜索竞争锁
                            if is_searching_for_usn.load(Ordering::SeqCst) {
                                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                                continue;
                            }

                            let volumes_list = {
                                let vm = vm_clone_for_usn.lock().await;
                                vm.volumes()
                            };

                            log::debug!(
                                "[USN] Polling {} volumes: {:?}, interval={}s",
                                volumes_list.len(),
                                volumes_list,
                                interval
                            );
                            for drive_letter in &volumes_list {
                                let dl_char = drive_letter.chars().next().unwrap_or('C');
                                log::debug!("[USN] Sending PollChanges for drive {}", dl_char);
                                usn_clone.poll_changes(dl_char, include_hidden, include_system);
                            }

                            tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
                        }
                    });
                }

                // 启动增量更新任务（所有用户）
                let vm_clone = vm.clone();
                let handle_clone = handle.clone();
                let is_searching_clone = is_searching.clone();
                let last_update_clone = last_index_update.clone();
                let mut incremental_done_rx = incremental_done_rx;

                tauri::async_runtime::spawn(async move {
                    let mut next_scan: Option<tokio::time::Instant> = None;

                    // 等待全量扫描完成后再开始增量更新
                    // 比固定 60 秒延迟更精确：全量扫描 30 秒完成则等 30 秒，5 分钟完成则等 5 分钟
                    if !*incremental_done_rx.borrow() {
                        log::info!("[Incremental] Waiting for full scan to complete...");
                        while incremental_done_rx.changed().await.is_ok() {
                            if *incremental_done_rx.borrow() {
                                break;
                            }
                        }
                    }
                    // 全量扫描完成后延迟 5 秒，让前端完成初始加载
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    log::info!("[Incremental] Starting incremental scan task");

                    loop {
                        // 与 USN 轮询间隔保持一致，避免 walkdir 增量扫描过于频繁
                        tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;

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

                        // 先获取卷列表（短暂持锁），并检查是否有需要 walkdir 扫描的非 USN 卷
                        let (volumes_list, has_non_usn) = {
                            let mut vm = vm_clone.lock().await;
                            let volumes_list = vm.volumes();
                            let has_non_usn = volumes_list.iter().any(|dl| {
                                vm.get_monitor_mut(dl).map(|m| !m.use_usn).unwrap_or(false)
                            });
                            (volumes_list, has_non_usn)
                        };

                        if !has_non_usn {
                            // 全部为 USN 卷，由 USN 轮询任务处理；本任务长睡眠避免空转
                            tokio::time::sleep(tokio::time::Duration::from_secs(300)).await;
                            continue;
                        }

                        for drive_letter in volumes_list {
                            // 如果有搜索请求，跳过剩余卷的扫描
                            if is_searching_clone.load(Ordering::SeqCst) {
                                log::info!("Incremental scan skipped: search in progress");
                                break;
                            }

                            // 步骤 1：短暂持锁 take monitor
                            let mut monitor = {
                                let mut vm = vm_clone.lock().await;
                                vm.take_monitor(&drive_letter)
                            };

                            if let Some(ref mut m) = monitor {
                                if m.use_usn {
                                    // USN volumes are handled by the USN polling task
                                    let mut vm = vm_clone.lock().await;
                                    vm.return_monitor(&drive_letter, monitor.take().unwrap());
                                    continue;
                                }
                                m.update_settings(include_hidden, include_system);
                            }

                            // 步骤 2：不持锁执行增量扫描（最耗时的部分）
                            let inc_result = if let Some(ref mut m) = monitor {
                                match m.scan_incremental(&handle_clone) {
                                    Ok(r) if r.added > 0 || r.updated > 0 || r.removed > 0 => {
                                        log::info!(
                                            "Incremental {}: +{} ~{} -{} (total: {})",
                                            drive_letter,
                                            r.added,
                                            r.updated,
                                            r.removed,
                                            r.total
                                        );
                                        Some(r)
                                    }
                                    _ => None,
                                }
                            } else {
                                None
                            };

                            // 步骤 3：短暂持锁 return monitor
                            if let Some(m) = monitor {
                                let mut vm = vm_clone.lock().await;
                                vm.return_monitor(&drive_letter, m);
                            }

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
                                // 通知前端刷新当前可见范围（文件已删除/修改/新增）
                                // payload 携带变化数量，便于前端判断是否真正有可见变化
                                let _ = handle_clone.emit(
                                    "records-refresh",
                                    serde_json::json!({
                                        "added": result.added,
                                        "updated": result.updated,
                                        "removed": result.removed,
                                    }),
                                );
                            }
                            // 锁已释放，前端请求可以继续
                        }

                        let now_str = chrono::Local::now().format("%H:%M:%S").to_string();
                        *last_update_clone.lock().await = now_str;
                        next_scan = Some(
                            tokio::time::Instant::now()
                                + tokio::time::Duration::from_secs(interval),
                        );
                    }
                });

                // 启动定期合并任务：每 10 分钟检查 delta 大小，超过 1 万条或 50MB 时触发 merge
                let vm_clone_merge = vm.clone();
                tauri::async_runtime::spawn(async move {
                    // 等待全量扫描完成后再启动合并任务
                    tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                    log::info!("[Merge] Periodic merge task started");

                    loop {
                        // 首次检查前等待 10 分钟
                        tokio::time::sleep(tokio::time::Duration::from_secs(600)).await;

                        let (delta_count, delta_memory) = {
                            let vm = vm_clone_merge.lock().await;
                            (vm.delta_count(), vm.delta_memory_bytes())
                        };
                        const DELTA_MEMORY_THRESHOLD: usize = 50 * 1024 * 1024; // 50 MB

                        if delta_count > 10_000 || delta_memory > DELTA_MEMORY_THRESHOLD {
                            log::info!(
                                "[Merge] Delta has {} entries / ~{} MB, triggering merge",
                                delta_count,
                                delta_memory / 1024 / 1024
                            );
                            let mut vm = vm_clone_merge.lock().await;
                            vm.merge_if_needed();
                        }
                    }
                });

                // 启动定期内存统计任务：每 5 分钟记录一次核心数据结构占用，便于排查内存波动
                let vm_clone_stats = vm.clone();
                tauri::async_runtime::spawn(async move {
                    // 等待全量扫描完成后再开始记录，避免扫描中数字剧烈变化
                    tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                    log::info!("[Memory] Periodic memory stats task started");

                    loop {
                        tokio::time::sleep(tokio::time::Duration::from_secs(300)).await;

                        let (files_bytes, path_table_bytes, fid_index_bytes, delta_bytes, sorted_indices_bytes) = {
                            let vm = vm_clone_stats.lock().await;
                            vm.memory_stats()
                        };
                        let total_mb =
                            (files_bytes + path_table_bytes + fid_index_bytes + delta_bytes + sorted_indices_bytes)
                                / 1024
                                / 1024;
                        log::info!(
                            "[Memory] files={} MB, path_table={} MB, fid_index={} MB, delta={} MB, sorted_indices={} MB, total={} MB",
                            files_bytes / 1024 / 1024,
                            path_table_bytes / 1024 / 1024,
                            fid_index_bytes / 1024 / 1024,
                            delta_bytes / 1024 / 1024,
                            sorted_indices_bytes / 1024 / 1024,
                            total_mb
                        );
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
            commands::search::get_records_range,
            commands::search::get_sorted_range,
            // 卷管理相关命令
            commands::volume::get_volumes,
            commands::volume::add_volume,
            commands::volume::remove_volume,
            commands::volume::refresh_volumes,
            commands::volume::get_index_status,
            commands::volume::get_monitored_volumes,
            // 文件操作相关命令
            commands::file::open_file,
            commands::file::open_folder,
            commands::file::delete_file,
            commands::file::copy_file,
            commands::file::move_file,
            commands::file::rename_file,
            // Shell 菜单相关命令
            commands::shell_menu::show_context_menu,
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
            file_logger::write_console(format!(
                "Tauri application exited with error: {}",
                e
            ));
            std::process::exit(1);
        });

    info!("Application exited normally");
}
