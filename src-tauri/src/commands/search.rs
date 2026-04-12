use crate::index::IndexManager;
use crate::search::{SearchOptions, SearchResult};
use serde::{Deserialize, Serialize};
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
    pub page: Option<usize>,
    pub page_size: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
    pub total: usize,
    pub page: usize,
    pub page_size: usize,
    pub total_pages: usize,
}

pub struct AppState {
    pub index_manager: IndexManager,
    pub volume_manager: Arc<Mutex<crate::index::monitor::VolumeManager>>,
}

#[tauri::command]
pub async fn search_files(
    state: State<'_, AppState>,
    params: SearchParams,
) -> Result<SearchResponse, String> {
    log::info!("Searching for: {}", params.query);

    let mut options = SearchOptions::default();
    
    if let Some(sort_by) = params.sort_by {
        options.sort_by = match sort_by.as_str() {
            "name" => crate::search::SortBy::Name,
            "path" => crate::search::SortBy::Path,
            "size" => crate::search::SortBy::Size,
            "modified" => crate::search::SortBy::ModifiedTime,
            "created" => crate::search::SortBy::CreatedTime,
            "accessed" => crate::search::SortBy::AccessedTime,
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

    let page = params.page.unwrap_or(1);
    let page_size = params.page_size.unwrap_or(100);
    let max_results = 1000000;

    let vm = state.volume_manager.lock().await;
    let mut all_results = vm.search_with_options(&params.query, &options);
    
    if all_results.len() > max_results {
        all_results.truncate(max_results);
    }
    
    let total = all_results.len();
    let total_pages = (total + page_size - 1) / page_size;
    
    let start = (page - 1) * page_size;
    let end = start + page_size;
    let paged_results: Vec<SearchResult> = all_results
        .into_iter()
        .skip(start)
        .take(page_size)
        .collect();
    
    Ok(SearchResponse {
        results: paged_results,
        total,
        page,
        page_size,
        total_pages,
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
        .map(|r| r.name)
        .take(limit)
        .collect();
    
    Ok(suggestions)
}