pub mod query;

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub file_id: u64,
    pub name: String,
    pub path: String,
    pub parent_id: u64,
    pub size: u64,
    pub created_time: DateTime<Local>,
    pub modified_time: DateTime<Local>,
    pub accessed_time: DateTime<Local>,
    pub is_directory: bool,
    pub attributes: u32,
    pub score: f32,
    pub formatted_size: String,
    pub formatted_created_time: String,
    pub formatted_modified_time: String,
    pub formatted_accessed_time: String,
}

impl SearchResult {
    pub fn new(file_id: u64, name: String, path: String) -> Self {
        let now = Local::now();
        let formatted_size = Self::format_size_static(0);
        let formatted_time = Self::format_time_static(&now);
        
        Self {
            file_id,
            name,
            path,
            parent_id: 0,
            size: 0,
            created_time: now,
            modified_time: now,
            accessed_time: now,
            is_directory: false,
            attributes: 0,
            score: 1.0,
            formatted_size,
            formatted_created_time: formatted_time.clone(),
            formatted_modified_time: formatted_time.clone(),
            formatted_accessed_time: formatted_time,
        }
    }
    
    pub fn format_size_static(size: u64) -> String {
        const KILOBYTE: u64 = 1024;
        const MEGABYTE: u64 = KILOBYTE * 1024;
        const GIGABYTE: u64 = MEGABYTE * 1024;
        
        match size {
            s if s >= GIGABYTE => format!("{:.1} GB", s as f64 / GIGABYTE as f64),
            s if s >= MEGABYTE => format!("{:.1} MB", s as f64 / MEGABYTE as f64),
            s if s >= KILOBYTE => format!("{:.1} KB", s as f64 / KILOBYTE as f64),
            s => format!("{} B", s),
        }
    }
    
    pub fn format_time_static(time: &DateTime<Local>) -> String {
        time.format("%Y-%m-%d %H:%M:%S").to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortBy {
    Name,
    Path,
    Size,
    ModifiedTime,
    CreatedTime,
    AccessedTime,
    Score,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    Ascending,
    Descending,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchOptions {
    pub sort_by: SortBy,
    pub sort_direction: SortDirection,
    pub limit: usize,
    pub load_all: bool,
    pub files_only: bool,
    pub directories_only: bool,
    pub case_sensitive: bool,
    pub match_full_path: bool,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            sort_by: SortBy::Score,
            sort_direction: SortDirection::Descending,
            limit: 1000000,
            load_all: false,
            files_only: false,
            directories_only: false,
            case_sensitive: false,
            match_full_path: false,
            min_size: None,
            max_size: None,
        }
    }
}