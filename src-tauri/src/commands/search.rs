use crate::index::UsnIndexManager;
use crate::search::{SearchOptions, SearchResult, SortBy, SortDirection};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::State;
use tokio::sync::Mutex;

fn parse_sort_by(s: &str) -> SortBy {
    match s {
        "name" => SortBy::Name,
        "path" => SortBy::Path,
        "size" => SortBy::Size,
        "modified" | "modified_time" => SortBy::ModifiedTime,
        _ => SortBy::Score,
    }
}

fn parse_sort_direction(s: &str) -> SortDirection {
    match s {
        "asc" => SortDirection::Ascending,
        _ => SortDirection::Descending,
    }
}

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
    pub volume_manager: Arc<Mutex<crate::index::monitor::VolumeManager>>,
    pub is_searching: Arc<AtomicBool>,
    pub last_index_update: Arc<Mutex<String>>,
    #[allow(dead_code)]
    pub usn_manager: Option<Arc<UsnIndexManager>>,
    pub scanning_volumes: Arc<Mutex<Vec<String>>>,
}

#[tauri::command]
pub async fn search_files(
    state: State<'_, AppState>,
    params: SearchParams,
) -> Result<SearchResponse, String> {
    log::info!("Searching for: {}", params.query);

    state.is_searching.store(true, Ordering::SeqCst);

    let mut options = SearchOptions::default();

    if let Some(ref sort_by) = params.sort_by {
        options.sort_by = parse_sort_by(sort_by);
    }

    if let Some(ref dir) = params.sort_direction {
        options.sort_direction = parse_sort_direction(dir);
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
    query: String,
    limit: usize,
) -> Result<Vec<String>, String> {
    let config = crate::config::Config::load().map_err(|e| e.to_string())?;
    let query_lower = query.to_lowercase();
    let suggestions: Vec<String> = config
        .search_suggestions
        .iter()
        .filter(|s| s.to_lowercase().contains(&query_lower))
        .take(limit)
        .cloned()
        .collect();
    Ok(suggestions)
}

#[tauri::command]
pub async fn add_search_suggestion(query: String) -> Result<(), String> {
    if query.trim().is_empty() {
        return Ok(());
    }
    let mut config = crate::config::Config::load().map_err(|e| e.to_string())?;
    // 去重：移除已有的相同项，插入到头部
    config.search_suggestions.retain(|s| s != &query);
    config.search_suggestions.insert(0, query);
    // 限制最大数量（复用 max_history_items）
    let max = config.max_history_items;
    config.search_suggestions.truncate(max);
    config.save().map_err(|e| e.to_string())?;
    Ok(())
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
    sort_by: String,
    sort_direction: String,
) -> Result<RecordsRangeResponse, String> {
    let t0 = std::time::Instant::now();
    let mut vm = state.volume_manager.lock().await;
    let lock_wait = t0.elapsed();

    if let Some((results, total)) = vm.get_cached_slice(
        parse_sort_by(&sort_by),
        parse_sort_direction(&sort_direction),
        start,
        end,
    ) {
        log::info!(
            "[CMD] range start={} end={} sort={}/{} total={} results={} lock_wait={:?} elapsed={:?}",
            start,
            end,
            sort_by,
            sort_direction,
            total,
            results.len(),
            lock_wait,
            t0.elapsed()
        );
        Ok(RecordsRangeResponse {
            results,
            total,
            start,
            end,
        })
    } else {
        log::warn!(
            "[CMD] range start={} end={} sort={}/{} cache expired",
            start,
            end,
            sort_by,
            sort_direction
        );
        Err("Cache expired or empty. Please search again.".to_string())
    }
}

#[tauri::command]
pub async fn get_sorted_range(
    state: State<'_, AppState>,
    sort_by: String,
    sort_direction: String,
    start: usize,
    end: usize,
) -> Result<RecordsRangeResponse, String> {
    let mut vm = state.volume_manager.lock().await;

    if let Some((results, total)) = vm.get_cached_slice(
        parse_sort_by(&sort_by),
        parse_sort_direction(&sort_direction),
        start,
        end,
    ) {
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
