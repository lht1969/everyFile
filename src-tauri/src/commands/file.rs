use std::process::Command;

#[tauri::command]
pub async fn open_file(path: String) -> Result<(), String> {
    log::info!("Opening file: {}", path);
    
    #[cfg(target_os = "windows")]
    {
        Command::new("cmd")
            .args(["/C", "start", "", &path])
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    
    Ok(())
}

#[tauri::command]
pub async fn open_folder(path: String) -> Result<(), String> {
    log::info!("Opening folder: {}", path);
    
    let folder_path = std::path::Path::new(&path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or(path.clone());
    
    #[cfg(target_os = "windows")]
    {
        Command::new("explorer")
            .arg(&folder_path)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    
    Ok(())
}

#[tauri::command]
pub async fn delete_file(path: String) -> Result<(), String> {
    log::info!("Deleting file: {}", path);
    
    std::fs::remove_file(&path).map_err(|e| e.to_string())?;
    
    Ok(())
}

#[tauri::command]
pub async fn copy_file(source: String, destination: String) -> Result<(), String> {
    log::info!("Copying file from {} to {}", source, destination);
    
    std::fs::copy(&source, &destination).map_err(|e| e.to_string())?;
    
    Ok(())
}

#[tauri::command]
pub async fn move_file(source: String, destination: String) -> Result<(), String> {
    log::info!("Moving file from {} to {}", source, destination);
    
    std::fs::rename(&source, &destination).map_err(|e| e.to_string())?;
    
    Ok(())
}