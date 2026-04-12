use crate::search::SearchResult;
use crate::commands::search::AppState;
use std::fs::File;
use std::io::Write;
use tauri::State;
use tokio::sync::Mutex;

#[tauri::command]
pub async fn export_csv(results: Vec<SearchResult>, path: String) -> Result<(), String> {
    log::info!("Exporting to CSV: {}", path);
    
    let mut file = File::create(&path).map_err(|e| e.to_string())?;
    
    writeln!(file, "Name,Path,Size,Created,Modified,Accessed,IsDirectory").map_err(|e| e.to_string())?;
    
    for r in results {
        writeln!(
            file,
            "{},{},{},{},{},{},{}",
            r.name,
            r.path,
            r.size,
            r.formatted_created_time,
            r.formatted_modified_time,
            r.formatted_accessed_time,
            r.is_directory
        ).map_err(|e| e.to_string())?;
    }
    
    Ok(())
}

#[tauri::command]
pub async fn export_txt(results: Vec<SearchResult>, path: String) -> Result<(), String> {
    log::info!("Exporting to TXT: {}", path);
    
    let mut file = File::create(&path).map_err(|e| e.to_string())?;
    
    for r in results {
        writeln!(file, "{}", r.path).map_err(|e| e.to_string())?;
    }
    
    Ok(())
}

#[tauri::command]
pub async fn export_json(results: Vec<SearchResult>, path: String) -> Result<(), String> {
    log::info!("Exporting to JSON: {}", path);
    
    let json = serde_json::to_string_pretty(&results).map_err(|e| e.to_string())?;
    
    let mut file = File::create(&path).map_err(|e| e.to_string())?;
    file.write_all(json.as_bytes()).map_err(|e| e.to_string())?;
    
    Ok(())
}

#[tauri::command]
pub async fn export_all_results(
    state: State<'_, super::search::AppState>,
    query: String,
    files_only: bool,
    directories_only: bool,
    format: String,
    path: String,
) -> Result<(), String> {
    log::info!("Exporting all results: query={}, format={}, path={}", query, format, path);
    
    let vm = state.volume_manager.lock().await;
    let results = vm.search_all(&query, files_only, directories_only);
    
    match format.as_str() {
        "csv" => {
            let mut file = File::create(&path).map_err(|e| e.to_string())?;
            writeln!(file, "Name,Path,Size,Created,Modified,Accessed,IsDirectory").map_err(|e| e.to_string())?;
            for r in results {
                writeln!(file, "{},{},{},{},{},{},{}", r.name, r.path, r.size, r.formatted_created_time, r.formatted_modified_time, r.formatted_accessed_time, r.is_directory).map_err(|e| e.to_string())?;
            }
        },
        "txt" => {
            let mut file = File::create(&path).map_err(|e| e.to_string())?;
            for r in results {
                writeln!(file, "{}", r.path).map_err(|e| e.to_string())?;
            }
        },
        "json" => {
            let json = serde_json::to_string_pretty(&results).map_err(|e| e.to_string())?;
            let mut file = File::create(&path).map_err(|e| e.to_string())?;
            file.write_all(json.as_bytes()).map_err(|e| e.to_string())?;
        },
        _ => return Err("Invalid format".to_string()),
    }
    
    Ok(())
}