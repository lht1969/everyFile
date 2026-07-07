use std::path::Path;
use std::process::Command;
use tauri::State;

#[tauri::command]
pub async fn open_file(path: String) -> Result<(), String> {
    log::info!("Opening file: {}", path);

    let path_clone = path.clone();
    tokio::task::spawn_blocking(move || {
        #[cfg(target_os = "windows")]
        {
            Command::new("cmd")
                .args(["/C", "start", "", &path_clone])
                .spawn()
                .map_err(|e| e.to_string())?;
        }
        Ok::<(), String>(())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn open_folder(path: String) -> Result<(), String> {
    log::info!("Opening folder: {}", path);

    let folder_path = if Path::new(&path).is_dir() {
        path.clone()
    } else {
        Path::new(&path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or(path.clone())
    };

    tokio::task::spawn_blocking(move || {
        #[cfg(target_os = "windows")]
        {
            Command::new("explorer")
                .arg(&folder_path)
                .spawn()
                .map_err(|e| e.to_string())?;
        }
        Ok::<(), String>(())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn delete_file(path: String, state: State<'_, super::search::AppState>) -> Result<(), String> {
    log::info!("Deleting file: {}", path);

    let path_clone = path.clone();
    tokio::task::spawn_blocking(move || {
        std::fs::remove_file(&path_clone).map_err(|e| e.to_string())?;
        Ok::<(), String>(())
    })
    .await
    .map_err(|e| e.to_string())??;

    let mut vm = state.volume_manager.lock().await;
    vm.remove_file(&path);

    Ok(())
}

#[tauri::command]
pub async fn copy_file(source: String, destination: String) -> Result<(), String> {
    log::info!("Copying file from {} to {}", source, destination);

    tokio::task::spawn_blocking(move || {
        std::fs::copy(&source, &destination).map_err(|e| e.to_string())?;
        Ok::<(), String>(())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn move_file(source: String, destination: String, state: State<'_, super::search::AppState>) -> Result<(), String> {
    log::info!("Moving file from {} to {}", source, destination);

    let source_clone = source.clone();
    tokio::task::spawn_blocking(move || {
        std::fs::rename(&source_clone, &destination).map_err(|e| e.to_string())?;
        Ok::<(), String>(())
    })
    .await
    .map_err(|e| e.to_string())??;

    let mut vm = state.volume_manager.lock().await;
    vm.remove_file(&source);

    Ok(())
}
