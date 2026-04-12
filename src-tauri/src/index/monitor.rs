use crate::error::Result;
use crate::search::{SearchOptions, SearchResult, SortDirection};
use std::collections::HashMap;
use tauri::Emitter;

pub struct VolumeManager {
    volumes: HashMap<String, VolumeMonitor>,
}

pub struct VolumeMonitor {
    drive_letter: String,
    files: Vec<SearchResult>,
}

impl VolumeManager {
    pub fn new() -> Self {
        Self {
            volumes: HashMap::new(),
        }
    }

    pub fn add_volume(&mut self, drive_letter: &str, _is_admin: bool) -> Result<()> {
        let monitor = VolumeMonitor::new(drive_letter.to_string());
        self.volumes.insert(drive_letter.to_string(), monitor);
        Ok(())
    }

    pub fn remove_volume(&mut self, drive_letter: &str) {
        self.volumes.remove(drive_letter);
    }

    pub fn volumes(&self) -> Vec<String> {
        self.volumes.keys().cloned().collect()
    }

    pub fn get_monitor_mut(&mut self, drive_letter: &str) -> Option<&mut VolumeMonitor> {
        self.volumes.get_mut(drive_letter)
    }

    pub fn take_monitor(&mut self, drive_letter: &str) -> Option<VolumeMonitor> {
        self.volumes.remove(drive_letter)
    }

    pub fn return_monitor(&mut self, drive_letter: &str, monitor: VolumeMonitor) {
        self.volumes.insert(drive_letter.to_string(), monitor);
    }

    pub fn total_file_count(&self) -> usize {
        self.volumes.values().map(|v| v.files.len()).sum()
    }

    pub fn get_file_count(&self, drive_letter: &str) -> usize {
        self.volumes
            .get(drive_letter)
            .map(|v| v.files.len())
            .unwrap_or(0)
    }

    pub fn get_all_files(&self) -> Vec<SearchResult> {
        self.volumes
            .values()
            .flat_map(|v| v.files.clone())
            .collect()
    }

    pub fn search_with_options(&self, query: &str, options: &SearchOptions) -> Vec<SearchResult> {
        log::info!(
            "search_with_options: query='{}', files_only={}, directories_only={}, volumes count={}",
            query,
            options.files_only,
            options.directories_only,
            self.volumes.len()
        );

        let parsed_query = crate::search::query::SearchQuery::parse(query);

        let mut results: Vec<SearchResult> = self
            .volumes
            .values()
            .flat_map(|v| v.search_with_query(&parsed_query))
            .collect();

        log::info!("After search_with_query: {} results", results.len());

        if options.files_only {
            let before = results.len();
            results.retain(|r| !r.is_directory);
            log::info!(
                "After files_only filter: {} (removed {})",
                results.len(),
                before - results.len()
            );
        }

        if options.directories_only {
            let before = results.len();
            results.retain(|r| r.is_directory);
            log::info!(
                "After directories_only filter: {} (removed {})",
                results.len(),
                before - results.len()
            );
        }

        self.sort_results(&mut results, options);

        if results.len() > options.limit {
            results.truncate(options.limit);
        }

        results
    }

    pub fn search_all(
        &self,
        query: &str,
        files_only: bool,
        directories_only: bool,
    ) -> Vec<SearchResult> {
        let parsed_query = crate::search::query::SearchQuery::parse(query);

        let mut results: Vec<SearchResult> = self
            .volumes
            .values()
            .flat_map(|v| v.search_with_query(&parsed_query))
            .collect();

        if files_only {
            results.retain(|r| !r.is_directory);
        }

        if directories_only {
            results.retain(|r| r.is_directory);
        }

        results.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

        results
    }

    fn sort_results(&self, results: &mut Vec<SearchResult>, options: &SearchOptions) {
        results.sort_by(|a, b| {
            let comparison = match options.sort_by {
                crate::search::SortBy::Name => a.name.cmp(&b.name),
                crate::search::SortBy::Path => a.path.cmp(&b.path),
                crate::search::SortBy::Size => a.size.cmp(&b.size),
                crate::search::SortBy::ModifiedTime => a.modified_time.cmp(&b.modified_time),
                crate::search::SortBy::CreatedTime => a.created_time.cmp(&b.created_time),
                crate::search::SortBy::AccessedTime => a.accessed_time.cmp(&b.accessed_time),
                crate::search::SortBy::Score => a
                    .score
                    .partial_cmp(&b.score)
                    .unwrap_or(std::cmp::Ordering::Equal),
            };

            match options.sort_direction {
                SortDirection::Ascending => comparison,
                SortDirection::Descending => comparison.reverse(),
            }
        });
    }

    pub fn scan_all(
        &mut self,
        _callback: Option<Box<dyn FnMut(usize, &str) + Send>>,
    ) -> Result<usize> {
        let mut total = 0;
        for (drive_letter, monitor) in self.volumes.iter_mut() {
            let count = monitor.scan()?;
            log::info!("Scanned volume {}: {} files", drive_letter, count);
            total += count;
        }
        Ok(total)
    }

    pub fn start_listening_all(&mut self) -> Result<()> {
        log::info!("Started listening to all volumes");
        Ok(())
    }

    pub fn stop_listening_all(&mut self) {
        log::info!("Stopped listening to all volumes");
    }

    pub fn process_all_events(&mut self) -> Result<usize> {
        Ok(0)
    }
}

impl VolumeMonitor {
    pub fn new(drive_letter: String) -> Self {
        Self {
            drive_letter,
            files: Vec::new(),
        }
    }

    pub fn scan(&mut self) -> Result<usize> {
        let path = format!("{}\\", self.drive_letter);

        self.files.clear();

        let walker = walkdir::WalkDir::new(&path)
            .max_depth(10)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                !name.starts_with('.') && !name.eq_ignore_ascii_case("$Recycle.Bin")
            });

        let mut count = 0;

        for entry in walker.filter_map(|e| e.ok()) {
            let metadata = entry.metadata().ok();
            let (size, is_dir, created, modified, accessed) = if let Some(ref m) = metadata {
                (
                    m.len(),
                    m.is_dir(),
                    m.created().ok(),
                    m.modified().ok(),
                    m.accessed().ok(),
                )
            } else {
                (0, false, None, None, None)
            };

            let now = chrono::Local::now();

            let created_time = created
                .map(|t| chrono::DateTime::<chrono::Utc>::from(t).with_timezone(&chrono::Local))
                .unwrap_or(now);

            let modified_time = modified
                .map(|t| chrono::DateTime::<chrono::Utc>::from(t).with_timezone(&chrono::Local))
                .unwrap_or(now);

            let accessed_time = accessed
                .map(|t| chrono::DateTime::<chrono::Utc>::from(t).with_timezone(&chrono::Local))
                .unwrap_or(now);

            let result = SearchResult {
                file_id: count as u64,
                name: entry.file_name().to_string_lossy().to_string(),
                path: entry.path().to_string_lossy().to_string(),
                parent_id: 0,
                size,
                created_time,
                modified_time,
                accessed_time,
                is_directory: is_dir,
                attributes: 0,
                score: 1.0,
                formatted_size: SearchResult::format_size_static(size),
                formatted_created_time: created_time.format("%Y-%m-%d %H:%M:%S").to_string(),
                formatted_modified_time: modified_time.format("%Y-%m-%d %H:%M:%S").to_string(),
                formatted_accessed_time: accessed_time.format("%Y-%m-%d %H:%M:%S").to_string(),
            };

            self.files.push(result);
            count += 1;
        }

        Ok(count)
    }

    pub fn scan_with_progress_callback(&mut self, handle: &tauri::AppHandle) -> Result<usize> {
        let path = format!("{}\\", self.drive_letter);

        self.files.clear();

        let walker = walkdir::WalkDir::new(&path)
            .max_depth(10)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                !name.starts_with('.') && !name.eq_ignore_ascii_case("$Recycle.Bin")
            });

        let mut count = 0;

        for entry in walker.filter_map(|e| e.ok()) {
            let metadata = entry.metadata().ok();
            let (size, is_dir, created, modified, accessed) = if let Some(ref m) = metadata {
                (
                    m.len(),
                    m.is_dir(),
                    m.created().ok(),
                    m.modified().ok(),
                    m.accessed().ok(),
                )
            } else {
                (0, false, None, None, None)
            };

            let now = chrono::Local::now();

            let created_time = created
                .map(|t| chrono::DateTime::<chrono::Utc>::from(t).with_timezone(&chrono::Local))
                .unwrap_or(now);

            let modified_time = modified
                .map(|t| chrono::DateTime::<chrono::Utc>::from(t).with_timezone(&chrono::Local))
                .unwrap_or(now);

            let accessed_time = accessed
                .map(|t| chrono::DateTime::<chrono::Utc>::from(t).with_timezone(&chrono::Local))
                .unwrap_or(now);

            let result = SearchResult {
                file_id: count as u64,
                name: entry.file_name().to_string_lossy().to_string(),
                path: entry.path().to_string_lossy().to_string(),
                parent_id: 0,
                size,
                created_time,
                modified_time,
                accessed_time,
                is_directory: is_dir,
                attributes: 0,
                score: 1.0,
                formatted_size: SearchResult::format_size_static(size),
                formatted_created_time: created_time.format("%Y-%m-%d %H:%M:%S").to_string(),
                formatted_modified_time: modified_time.format("%Y-%m-%d %H:%M:%S").to_string(),
                formatted_accessed_time: accessed_time.format("%Y-%m-%d %H:%M:%S").to_string(),
            };

            self.files.push(result);
            count += 1;

            if count > 0 && count % 20000 == 0 {
                let _ = handle.emit(
                    "scan-progress",
                    serde_json::json!({
                        "volume": self.drive_letter,
                        "count": count
                    }),
                );
            }
        }

        Ok(count)
    }

    pub fn scan_with_progress(
        &mut self,
        _callback: Option<Box<dyn FnMut(usize, &str) + Send>>,
    ) -> Result<usize> {
        self.scan()
    }

    pub fn get_all_files(&self) -> Vec<SearchResult> {
        self.files.clone()
    }

    pub fn clear_index(&mut self) {
        self.files.clear();
    }

    fn search(&self, query: &str) -> Vec<SearchResult> {
        let query_lower = query.to_lowercase();

        self.files
            .iter()
            .filter(|f| {
                f.name.to_lowercase().contains(&query_lower)
                    || f.path.to_lowercase().contains(&query_lower)
            })
            .cloned()
            .collect()
    }

    fn search_with_query(&self, query: &crate::search::query::SearchQuery) -> Vec<SearchResult> {
        self.files
            .iter()
            .filter(|f| {
                if !query.keywords.is_empty() {
                    let name_lower = f.name.to_lowercase();
                    let _path_lower = f.path.to_lowercase();
                    let mut matched = false;
                    for keyword in &query.keywords {
                        let kw_lower = keyword.to_lowercase();
                        if name_lower.contains(&kw_lower) {
                            matched = true;
                            break;
                        }
                    }
                    if !matched {
                        return false;
                    }
                }

                if let Some(ref size_filter) = query.size_filter {
                    if !size_filter.matches(f.size) {
                        return false;
                    }
                }

                if let Some(ref date_filter) = query.date_filter {
                    let time = match date_filter.date_type {
                        crate::search::query::DateType::Created => &f.created_time,
                        crate::search::query::DateType::Modified => &f.modified_time,
                        crate::search::query::DateType::Accessed => &f.accessed_time,
                    };
                    if let Some(ref start) = date_filter.start {
                        if time < start {
                            return false;
                        }
                    }
                    if let Some(ref end) = date_filter.end {
                        if time > end {
                            return false;
                        }
                    }
                }

                if let Some(ref path_filter) = query.path_filter {
                    if !f.path.to_lowercase().contains(&path_filter.to_lowercase()) {
                        return false;
                    }
                }

                if let Some(ref regex_pattern) = query.regex_pattern {
                    if !regex_pattern.is_match(&f.name) {
                        return false;
                    }
                }

                true
            })
            .cloned()
            .collect()
    }
}
