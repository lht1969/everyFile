use crate::fs::{get_all_volumes, VolumeInfo};
use serde::{Deserialize, Serialize};
use tauri::Emitter;
use tauri::State;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeStatus {
    pub drive_letter: String,
    pub state: VolumeState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum VolumeState {
    Loading,
    Ready { file_count: usize },
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeResponse {
    pub drive_letter: String,
    pub volume_name: String,
    pub file_system: String,
    pub total_size: u64,
    pub free_space: u64,
    pub file_count: usize,
}

impl From<VolumeInfo> for VolumeResponse {
    fn from(v: VolumeInfo) -> Self {
        Self {
            drive_letter: v.drive_letter,
            volume_name: v.volume_name,
            file_system: v.file_system,
            total_size: v.total_size,
            free_space: v.free_space,
            file_count: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexStatusResponse {
    pub status: String,
    pub file_count: usize,
    pub progress: f32,
    pub message: String,
    pub volumes: Vec<String>,
    pub last_update: String,
    pub scanning_volumes: Vec<String>,
    pub volume_statuses: Vec<VolumeStatus>,
}

#[tauri::command]
pub async fn get_volumes() -> Result<Vec<VolumeResponse>, String> {
    let volumes = get_all_volumes().map_err(|e| e.to_string())?;
    let mut result: Vec<VolumeResponse> = volumes.into_iter().map(VolumeResponse::from).collect();
    result.sort_by(|a, b| a.drive_letter.cmp(&b.drive_letter));
    Ok(result)
}

#[tauri::command]
pub async fn add_volume(
    app_handle: tauri::AppHandle,
    state: State<'_, super::search::AppState>,
    volume: String,
) -> Result<(), String> {
    log::info!("Adding volume: {}", volume);

    let is_admin = crate::fs::is_elevated();
    // 加载配置，获取索引设置
    let config = crate::config::Config::load().ok();
    let include_hidden_files = config
        .as_ref()
        .map(|c| c.index_settings.include_hidden_files)
        .unwrap_or(false);
    let include_system_files = config
        .as_ref()
        .map(|c| c.index_settings.include_system_files)
        .unwrap_or(false);

    // 查询卷的文件系统类型
    let file_system = crate::fs::get_volume_info(&volume)
        .ok()
        .map(|info| info.file_system)
        .unwrap_or_default();
    let is_ntfs = file_system.eq_ignore_ascii_case("NTFS");

    // === 立即操作：注册卷 + 保存配置 ===
    {
        let mut vm = state.volume_manager.lock().await;
        vm.add_volume(
            &volume,
            is_admin && is_ntfs,  // 仅 NTFS 卷在管理员模式下启用 USN
            include_hidden_files,
            include_system_files,
        )
        .map_err(|e| e.to_string())?;

        // 设置文件系统类型
        if let Some(monitor) = vm.get_monitor_mut(&volume) {
            monitor.file_system = file_system.clone();
        }

        if is_admin && is_ntfs {
            if let Some(monitor) = vm.get_monitor_mut(&volume) {
                monitor.use_usn = true;
            }
        }
    }

    if let Ok(mut config) = crate::config::Config::load() {
        if !config.monitored_volumes.contains(&volume) {
            config.monitored_volumes.push(volume.clone());
            if let Err(e) = config.save() {
                log::warn!("Failed to save config after adding volume: {:?}", e);
            }
        }
    }

    // === 后台异步扫描 ===
    let volume_clone = volume.clone();
    let vm = state.volume_manager.clone();
    let scanning_volumes = state.scanning_volumes.clone();
    let usn_manager = state.usn_manager.clone();

    tokio::spawn(async move {
        // 标记扫描中
        {
            let mut sv = scanning_volumes.lock().await;
            if !sv.contains(&volume_clone) {
                sv.push(volume_clone.clone());
            }
        }

        // 清除之前的扫描错误
        {
            let mut vm = vm.lock().await;
            vm.clear_scan_error(&volume_clone);
        }

        // 执行扫描
        let scan_result = if is_admin && is_ntfs {
            let usn_guard = usn_manager.lock().await;
            if let Some(ref usn) = *usn_guard {
                let dl_char = volume_clone.chars().next().unwrap_or('C');
                log::info!("[USN] Adding volume: dispatching full scan for {}", dl_char);
                usn.full_scan(dl_char, include_hidden_files, include_system_files);
                // USN scan is dispatched via channel; treat dispatch as success
                Ok(0)
            } else {
                // fallback: walkdir 扫描
                log::info!("Volume {} ({}) using walkdir scan (no USN manager)", volume_clone, file_system);
                let mut vm = vm.lock().await;
                if let Some(mut monitor) = vm.take_monitor(&volume_clone) {
                    let result = monitor.scan();
                    vm.return_monitor(&volume_clone, monitor);
                    result
                } else {
                    Ok(0)
                }
            }
        } else {
            // 非管理员模式或非 NTFS：使用 walkdir 扫描
            log::info!("Volume {} ({}) using walkdir scan", volume_clone, file_system);
            let mut vm = vm.lock().await;
            if let Some(mut monitor) = vm.take_monitor(&volume_clone) {
                let result = monitor.scan();
                vm.return_monitor(&volume_clone, monitor);
                result
            } else {
                Ok(0)
            }
        };

        // 记录错误
        match scan_result {
            Ok(count) => {
                log::info!("Scanned new volume {}: {} files", volume_clone, count);
                let _ = app_handle.emit(
                    "scan-complete",
                    serde_json::json!({
                        "volume": volume_clone,
                        "count": count
                    }),
                );
            }
            Err(e) => {
                log::warn!("Failed to scan new volume {}: {:?}", volume_clone, e);
                let mut vm = vm.lock().await;
                vm.set_scan_error(&volume_clone, e.to_string());
            }
        }

        // 从扫描中卷列表移除
        {
            let mut sv = scanning_volumes.lock().await;
            sv.retain(|v| v != &volume_clone);
        }

        let _ = app_handle.emit("index-updated", ());
    });

    Ok(())
}

#[tauri::command]
pub async fn remove_volume(
    state: State<'_, super::search::AppState>,
    volume: String,
) -> Result<(), String> {
    log::info!("Removing volume: {}", volume);

    let mut vm = state.volume_manager.lock().await;
    vm.remove_volume(&volume);

    if let Ok(mut config) = crate::config::Config::load() {
        config.monitored_volumes.retain(|v| v != &volume);
        if let Err(e) = config.save() {
            log::warn!("Failed to save config after removing volume: {:?}", e);
        }
    }

    Ok(())
}

#[tauri::command]
pub async fn refresh_volumes(
    state: State<'_, super::search::AppState>,
) -> Result<Vec<VolumeResponse>, String> {
    let mut vm = state.volume_manager.lock().await;

    let volumes = get_all_volumes().map_err(|e| e.to_string())?;

    // 加载配置，获取索引设置
    let config = crate::config::Config::load().ok();
    let include_hidden_files = config
        .as_ref()
        .map(|c| c.index_settings.include_hidden_files)
        .unwrap_or(false);
    let include_system_files = config
        .as_ref()
        .map(|c| c.index_settings.include_system_files)
        .unwrap_or(false);

    for volume in &volumes {
        if !vm.volumes().contains(&volume.drive_letter) {
            let is_admin = crate::fs::is_elevated();
            let _ = vm.add_volume(
                &volume.drive_letter,
                is_admin,
                include_hidden_files,
                include_system_files,
            );
        }
    }

    let mut result: Vec<VolumeResponse> = volumes.into_iter().map(VolumeResponse::from).collect();
    result.sort_by(|a, b| a.drive_letter.cmp(&b.drive_letter));
    Ok(result)
}

#[tauri::command]
pub async fn get_index_status(
    state: State<'_, super::search::AppState>,
) -> Result<IndexStatusResponse, String> {
    let vm = state.volume_manager.lock().await;
    let file_count = vm.total_file_count();
    let mut volumes = vm.volumes();
    volumes.sort();
    let last_update = state.last_index_update.lock().await.clone();
    let scanning_volumes = state.scanning_volumes.lock().await.clone();

    let message = if volumes.is_empty() {
        "等待扫描...".to_string()
    } else if !scanning_volumes.is_empty() {
        format!("{} 加载中...", scanning_volumes[0])
    } else {
        "就绪".to_string()
    };

    let volume_statuses: Vec<VolumeStatus> = volumes.iter().map(|v| {
        let state = if scanning_volumes.contains(v) {
            VolumeState::Loading
        } else if let Some(msg) = vm.scan_errors.get(v) {
            VolumeState::Error { message: msg.clone() }
        } else {
            VolumeState::Ready { file_count: vm.get_file_count(v) }
        };
        VolumeStatus {
            drive_letter: v.clone(),
            state,
        }
    }).collect();

    Ok(IndexStatusResponse {
        status: if scanning_volumes.is_empty() { "ready".to_string() } else { "scanning".to_string() },
        file_count,
        progress: if scanning_volumes.is_empty() { 1.0 } else { 0.0 },
        message,
        volumes,
        last_update,
        scanning_volumes,
        volume_statuses,
    })
}

#[tauri::command]
pub async fn get_monitored_volumes(
    state: State<'_, super::search::AppState>,
) -> Result<Vec<VolumeResponse>, String> {
    let vm = state.volume_manager.lock().await;
    let mut volumes = vm.volumes();
    // 始终以字母顺序返回卷列表
    volumes.sort();
    let mut result = Vec::new();

    for vol in volumes {
        let count = vm.get_file_count(&vol);
        let file_system = vm.get_monitor(&vol)
            .map(|m| m.file_system.clone())
            .unwrap_or_default();
        result.push(VolumeResponse {
            drive_letter: vol.clone(),
            volume_name: vol.clone(),
            file_system,
            total_size: 0,
            free_space: 0,
            file_count: count,
        });
    }

    Ok(result)
}
