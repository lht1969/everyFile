use crate::index::IndexManager;
use crate::search::{SearchOptions, SearchResult};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::State;
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchParams {
    pub query: String,
    pub sort_by: Option<String>,
    pub sort_direction: Option<String>,
    pub files_only: Option<bool>,
    pub directories_only: Option<bool>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub total: usize,
    pub results: Vec<SearchResult>,
}

pub struct AppState {
    pub index_manager: IndexManager,
    pub volume_manager: Arc<Mutex<crate::index::monitor::VolumeManager>>,
    pub is_searching: Arc<AtomicBool>,
    pub last_index_update: Arc<Mutex<String>>,
}

#[tauri::command]
pub async fn search_files(
    state: State<'_, AppState>,
    params: SearchParams,
) -> Result<SearchResponse, String> {
    log::info!("Searching for: {}", params.query);

    state.is_searching.store(true, Ordering::SeqCst);

    let mut options = SearchOptions::default();

    if let Some(sort_by) = params.sort_by {
        options.sort_by = match sort_by.as_str() {
            "name" => crate::search::SortBy::Name,
            "path" => crate::search::SortBy::Path,
            "size" => crate::search::SortBy::Size,
            "modified" | "modified_time" => crate::search::SortBy::ModifiedTime,
            _ => crate::search::SortBy::Score,
        };
    }

    if let Some(dir) = params.sort_direction {
        options.sort_direction = match dir.as_str() {
            "asc" => crate::search::SortDirection::Ascending,
            _ => crate::search::SortDirection::Descending,
        };
    }

    options.files_only = params.files_only.unwrap_or(true);
    options.directories_only = params.directories_only.unwrap_or(false);
    options.min_size = params.min_size;
    options.max_size = params.max_size;

    let mut vm = state.volume_manager.lock().await;
    let (all_results, total) = vm.search_with_options(&params.query, &options);

    let first_batch: Vec<SearchResult> = all_results.into_iter().take(50).collect();

    state.is_searching.store(false, Ordering::SeqCst);

    Ok(SearchResponse {
        total,
        results: first_batch,
    })
}

#[tauri::command]
pub async fn get_search_suggestions(
    state: State<'_, AppState>,
    query: String,
    limit: usize,
) -> Result<Vec<String>, String> {
    let results = state.index_manager.search(&query, limit, 0)
        .await
        .map_err(|e| e.to_string())?;

    let suggestions: Vec<String> = results
        .into_iter()
        .map(|r| r.name.into())
        .take(limit)
        .collect();

    Ok(suggestions)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordsRangeResponse {
    pub results: Vec<SearchResult>,
    pub total: usize,
    pub start: usize,
    pub end: usize,
}

#[tauri::command]
pub async fn get_records_range(
    state: State<'_, AppState>,
    start: usize,
    end: usize,
) -> Result<RecordsRangeResponse, String> {
    let vm = state.volume_manager.lock().await;

    if let Some((results, total)) = vm.get_cached_slice(start, end) {
        Ok(RecordsRangeResponse {
            results,
            total,
            start,
            end,
        })
    } else {
        Err("Cache expired or empty. Please search again.".to_string())
    }
}
