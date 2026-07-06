use crate::fs::{get_ntfs_volumes, VolumeInfo};
use serde::{Deserialize, Serialize};
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
}

#[tauri::command]
pub async fn get_volumes() -> Result<Vec<VolumeResponse>, String> {
    let volumes = get_ntfs_volumes().map_err(|e| e.to_string())?;
    Ok(volumes.into_iter().map(VolumeResponse::from).collect())
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
    let include_hidden_files = config.as_ref().map(|c| c.index_settings.include_hidden_files).unwrap_or(false);
    let include_system_files = config.as_ref().map(|c| c.index_settings.include_system_files).unwrap_or(false);
    
    let mut vm = state.volume_manager.lock().await;
    vm.add_volume(&volume, is_admin, include_hidden_files, include_system_files).map_err(|e| e.to_string())?;
    
    if let Some(mut monitor) = vm.take_monitor(&volume) {
        let _ = monitor.scan();
        vm.return_monitor(&volume, monitor);
    }
    
    if let Ok(mut config) = crate::config::Config::load() {
        if !config.monitored_volumes.contains(&volume) {
            config.monitored_volumes.push(volume.clone());
            let _ = config.save();
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
        let _ = config.save();
    }
    
    Ok(())
}

#[tauri::command]
pub async fn refresh_volumes(
    state: State<'_, super::search::AppState>,
) -> Result<Vec<VolumeResponse>, String> {
    let mut vm = state.volume_manager.lock().await;
    
    let volumes = get_ntfs_volumes().map_err(|e| e.to_string())?;
    
    // 加载配置，获取索引设置
    let config = crate::config::Config::load().ok();
    let include_hidden_files = config.as_ref().map(|c| c.index_settings.include_hidden_files).unwrap_or(false);
    let include_system_files = config.as_ref().map(|c| c.index_settings.include_system_files).unwrap_or(false);
    
    for volume in &volumes {
        if !vm.volumes().contains(&volume.drive_letter) {
            let is_admin = crate::fs::is_elevated();
            let _ = vm.add_volume(&volume.drive_letter, is_admin, include_hidden_files, include_system_files);
        }
    }
    
    Ok(volumes.into_iter().map(VolumeResponse::from).collect())
}

#[tauri::command]
pub async fn rebuild_index(
    state: State<'_, super::search::AppState>,
) -> Result<(), String> {
    log::info!("Rebuilding index...");
    
    let mut vm = state.volume_manager.lock().await;
    vm.scan_all(None).map_err(|e| e.to_string())?;
    
    Ok(())
}

#[tauri::command]
pub async fn get_index_status(
    state: State<'_, super::search::AppState>,
) -> Result<IndexStatusResponse, String> {
    let vm = state.volume_manager.lock().await;
    let file_count = vm.total_file_count();
    let volumes = vm.volumes();
    let last_update = state.last_index_update.lock().await.clone();

    let message = if volumes.is_empty() {
        "等待扫描...".to_string()
    } else {
        "就绪".to_string()
    };

    Ok(IndexStatusResponse {
        status: "ready".to_string(),
        file_count,
        progress: 1.0,
        message,
        volumes,
        last_update,
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
        result.push(VolumeResponse {
            drive_letter: vol.clone(),
            volume_name: vol.clone(),
            file_system: "NTFS".to_string(),
            total_size: 0,
            free_space: 0,
            file_count: count,
        });
    }
    
    Ok(result)
}