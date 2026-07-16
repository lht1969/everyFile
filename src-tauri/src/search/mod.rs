pub mod query;

use chrono::{DateTime, TimeZone, Utc};
use compact_str::CompactString;
use serde::{Deserialize, Serialize};

/// 对外传输的搜索结果（序列化给前端）
///
/// 保持完整 path 字段以兼容前端接口。
/// 内部存储使用 FileEntry（紧凑结构），返回前端时通过 PathTable 转换。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub name: CompactString,
    pub path: CompactString,
    pub size: u64,
    pub modified_time: i64,
    pub file_id: u32,
    pub is_directory: bool,
}

/// 内部紧凑存储的文件条目
///
/// 相比 SearchResult 的优化：
/// 1. 用 path_id (u32, 4字节) 替代 path (CompactString, 24字节 + 堆分配 ~100字节)
///    - 通过 PathTable 按需解析完整路径
/// 2. modified_time 用 i32 (4字节) 替代 i64 (8字节)
///    - i32 Unix 秒可覆盖到 2038 年，足够当前使用
///
/// 结构体大小对比：
/// - SearchResult: 72 字节 + 路径堆分配 ~116 字节 = ~188 字节
/// - FileEntry:    48 字节（无路径堆分配）
/// - 节省约 74%
pub struct FileEntry {
    pub name: CompactString,       // 24 字节 (inline，短名称不分配)
    pub size: u64,                 // 8 字节
    pub path_id: u32,              // 4 字节 (PathTable 中的路径 ID)
    pub file_id: u32,              // 4 字节 (MFT record number)
    pub modified_time: i32,        // 4 字节 (Unix 秒)
    pub is_directory: bool,        // 1 字节 + 3 字节 padding
}

impl SearchResult {
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

impl FileEntry {
    /// 从 FileEntry 和完整路径构建 SearchResult（用于返回前端）
    ///
    /// modified_time 从 i32 提升为 i64 以兼容前端接口
    #[inline]
    pub fn to_search_result(&self, full_path: CompactString) -> SearchResult {
        SearchResult {
            name: self.name.clone(),
            path: full_path,
            size: self.size,
            modified_time: self.modified_time as i64,
            file_id: self.file_id,
            is_directory: self.is_directory,
        }
    }

    /// 从 i64 时间戳创建 FileEntry（用于 USN worker 构建）
    #[inline]
    pub fn new(
        name: CompactString,
        path_id: u32,
        size: u64,
        modified_time: i64,
        file_id: u32,
        is_directory: bool,
    ) -> Self {
        Self {
            name,
            path_id,
            size,
            modified_time: modified_time as i32,
            file_id,
            is_directory,
        }
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
