use crate::fs::{get_all_volumes, VolumeInfo};
use serde::{Deserialize, Serialize};
use tauri::Emitter;
use tauri::State;

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
        // 管理员模式 + NTFS：标记 USN 并通过 USN worker 全量扫描
        if let Some(monitor) = vm.get_monitor_mut(&volume) {
            monitor.use_usn = true;
        }
        if let Some(ref usn) = state.usn_manager {
            let dl_char = volume.chars().next().unwrap_or('C');
            drop(vm);
            log::info!("[USN] Adding volume: dispatching full scan for {}", dl_char);
            usn.full_scan(dl_char, include_hidden_files, include_system_files);
        } else {
            // fallback: walkdir 扫描
            if let Some(mut monitor) = vm.take_monitor(&volume) {
                if let Err(e) = monitor.scan() {
                    log::warn!("Failed to scan new volume {}: {:?}", volume, e);
                }
                vm.return_monitor(&volume, monitor);
            }
        }
    } else {
        // 非管理员模式或非 NTFS：使用 walkdir 扫描
        log::info!("Volume {} ({}) using walkdir scan", volume, file_system);
        if let Some(mut monitor) = vm.take_monitor(&volume) {
            if let Err(e) = monitor.scan() {
                log::warn!("Failed to scan new volume {}: {:?}", volume, e);
            }
            vm.return_monitor(&volume, monitor);
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
pub async fn rebuild_index(
    app_handle: tauri::AppHandle,
    state: State<'_, super::search::AppState>,
) -> Result<(), String> {
    log::info!("Rebuilding index...");

    let config = crate::config::Config::load().ok();
    let include_hidden_files = config
        .as_ref()
        .map(|c| c.index_settings.include_hidden_files)
        .unwrap_or(false);
    let include_system_files = config
        .as_ref()
        .map(|c| c.index_settings.include_system_files)
        .unwrap_or(false);
    log::info!(
        "Rebuild with include_hidden_files={}, include_system_files={}",
        include_hidden_files,
        include_system_files
    );

    let is_admin = crate::fs::is_elevated();

    // 在重建索引前，将当前配置的隐藏/系统文件设置同步到已加载的卷监视器
    {
        let mut vm = state.volume_manager.lock().await;
        for letter in vm.volumes() {
            if let Some(monitor) = vm.get_monitor_mut(&letter) {
                monitor.update_settings(include_hidden_files, include_system_files);
            }
        }
    }

    if is_admin {
        // 管理员模式：通过 USN worker 进行全量扫描（与启动流程一致）
        if let Some(ref usn) = state.usn_manager {
            let volumes = {
                let vm = state.volume_manager.lock().await;
                vm.volumes()
            };
            // 标记所有卷为扫描中
            {
                let mut sv = state.scanning_volumes.lock().await;
                *sv = volumes.clone();
            }
            for drive_letter in volumes {
                let dl_char = drive_letter.chars().next().unwrap_or('C');
                log::info!(
                    "[USN] Rebuild: issuing full scan for drive {} (hidden={}, system={})",
                    dl_char,
                    include_hidden_files,
                    include_system_files
                );
                usn.full_scan(dl_char, include_hidden_files, include_system_files);
            }
        } else {
            log::warn!("Admin mode but no USN manager, falling back to walkdir");
            let volumes = {
                let vm = state.volume_manager.lock().await;
                vm.volumes()
            };
            {
                let mut sv = state.scanning_volumes.lock().await;
                *sv = volumes;
            }
            let mut vm = state.volume_manager.lock().await;
            vm.scan_all_with_progress(&app_handle)
                .map_err(|e| e.to_string())?;
            drop(vm);
            state.scanning_volumes.lock().await.clear();
        }
    } else {
        // 非管理员模式：使用 walkdir 扫描
        let volumes = {
            let vm = state.volume_manager.lock().await;
            vm.volumes()
        };
        {
            let mut sv = state.scanning_volumes.lock().await;
            *sv = volumes;
        }
        let mut vm = state.volume_manager.lock().await;
        vm.scan_all_with_progress(&app_handle)
            .map_err(|e| e.to_string())?;
        drop(vm);
        state.scanning_volumes.lock().await.clear();
    }

    let _ = app_handle.emit("rebuild-complete", ());

    Ok(())
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

    Ok(IndexStatusResponse {
        status: if scanning_volumes.is_empty() { "ready".to_string() } else { "scanning".to_string() },
        file_count,
        progress: if scanning_volumes.is_empty() { 1.0 } else { 0.0 },
        message,
        volumes,
        last_update,
        scanning_volumes,
    })
}

#[tauri::command]
pub async fn get_monitored_volumes(
    state: State<'_, super::search::AppState>,
) -> Result<Vec<VolumeResponse>, String> {
    let vm = state.volume_manager.lock().await;
    let volumes = vm.volumes();
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
