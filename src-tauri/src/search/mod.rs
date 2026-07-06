pub mod query;

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub file_id: u64,
    pub name: Box<str>,
    pub path: Box<str>,
    pub size: u64,
    pub modified_time: i64,
    pub is_directory: bool,
}

impl SearchResult {
    #[inline]
    pub fn name_lower(&self) -> String {
        self.name.to_lowercase()
    }

    #[inline]
    pub fn path_lower(&self) -> String {
        self.path.to_lowercase()
    }

    #[inline]
    pub fn formatted_size(&self) -> String {
        Self::format_size_static(self.size)
    }

    #[inline]
    pub fn formatted_modified_time(&self) -> String {
        let dt: DateTime<Utc> = Utc.timestamp_opt(self.modified_time, 0).single().unwrap_or_default();
        dt.format("%Y/%m/%d %H:%M:%S").to_string()
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

    pub fn format_time_static(timestamp: i64) -> String {
        let dt: DateTime<Utc> = Utc.timestamp_opt(timestamp, 0).single().unwrap_or_default();
        dt.format("%Y/%m/%d %H:%M:%S").to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortBy {
    Name,
    Path,
    Size,
    ModifiedTime,
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
