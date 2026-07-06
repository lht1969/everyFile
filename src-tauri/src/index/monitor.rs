use crate::error::Result;
use crate::search::{SearchOptions, SearchResult, SortBy, SortDirection};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tauri::Emitter;

const CACHE_TTL_SECS: u64 = 30;

pub struct SearchCache {
    pub query: String,
    pub sort_by: SortBy,
    pub sort_direction: SortDirection,
    pub files_only: bool,
    pub directories_only: bool,
    pub matched: Vec<(String, usize)>,
    pub total: usize,
    pub created_at: Instant,
}

impl SearchCache {
    pub fn is_valid(&self) -> bool {
        self.created_at.elapsed() < Duration::from_secs(CACHE_TTL_SECS)
    }

    pub fn get_slice(&self, volumes: &HashMap<String, VolumeMonitor>, start: usize, end: usize) -> Vec<SearchResult> {
        let end = end.min(self.matched.len());
        if start >= self.matched.len() || start >= end {
            return Vec::new();
        }
        self.matched[start..end].iter()
            .filter_map(|(vol, idx)| {
                volumes.get(vol).and_then(|m| m.files.get(*idx)).cloned()
            })
            .collect()
    }
}

pub struct VolumeManager {
    volumes: HashMap<String, VolumeMonitor>,
    pub search_cache: Option<SearchCache>,
}

pub struct VolumeMonitor {
    drive_letter: String,
    files: Vec<SearchResult>,
    include_hidden_files: bool,
    include_system_files: bool,
}

impl VolumeManager {
    pub fn new() -> Self {
        Self {
            volumes: HashMap::new(),
            search_cache: None,
        }
    }

    pub fn add_volume(&mut self, drive_letter: &str, _is_admin: bool, include_hidden_files: bool, include_system_files: bool) -> Result<()> {
        let monitor = VolumeMonitor::new(drive_letter.to_string(), include_hidden_files, include_system_files);
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

    pub fn search_with_options(&mut self, query: &str, options: &SearchOptions) -> (Vec<SearchResult>, usize) {
        log::info!(
            "search_with_options: query='{}', files_only={}, directories_only={}, volumes count={}",
            query,
            options.files_only,
            options.directories_only,
            self.volumes.len()
        );

        let mut matched: Vec<(String, usize)> = Vec::new();

        for (vol_key, monitor) in &self.volumes {
            if query.trim().is_empty() {
                for (idx, file) in monitor.files.iter().enumerate() {
                    if options.files_only && file.is_directory { continue; }
                    if options.directories_only && !file.is_directory { continue; }
                    matched.push((vol_key.clone(), idx));
                }
            } else {
                let parsed_query = crate::search::query::SearchQuery::parse(query);
                for (idx, file) in monitor.files.iter().enumerate() {
                    if !crate::search::query::SearchQuery::matches(&parsed_query, file) { continue; }
                    if options.files_only && file.is_directory { continue; }
                    if options.directories_only && !file.is_directory { continue; }
                    matched.push((vol_key.clone(), idx));
                }
            }
        }

        let total = matched.len();
        log::info!("Matched: {} files", total);

        matched.sort_by(|a, b| {
            let fa = &self.volumes[&a.0].files[a.1];
            let fb = &self.volumes[&b.0].files[b.1];
            self.compare_for_sort(fa, fb, options)
        });

        let first_batch: Vec<SearchResult> = matched.iter().take(50)
            .filter_map(|(vol, idx)| self.volumes.get(vol).and_then(|m| m.files.get(*idx)).cloned())
            .collect();

        self.search_cache = Some(SearchCache {
            query: query.to_string(),
            sort_by: options.sort_by,
            sort_direction: options.sort_direction,
            files_only: options.files_only,
            directories_only: options.directories_only,
            matched,
            total,
            created_at: Instant::now(),
        });

        (first_batch, total)
    }

    fn compare_for_sort(&self, a: &SearchResult, b: &SearchResult, options: &SearchOptions) -> std::cmp::Ordering {
        let cmp = match options.sort_by {
            SortBy::Name => a.name.cmp(&b.name),
            SortBy::Path => a.path.cmp(&b.path),
            SortBy::Size => a.size.cmp(&b.size),
            SortBy::ModifiedTime => a.modified_time.cmp(&b.modified_time),
            SortBy::Score => std::cmp::Ordering::Equal,
        };
        match options.sort_direction {
            SortDirection::Ascending => cmp,
            SortDirection::Descending => cmp.reverse(),
        }
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
        self.search_cache = None;
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

    pub fn remove_file(&mut self, file_path: &str) {
        for monitor in self.volumes.values_mut() {
            monitor.remove_file(file_path);
        }
    }

    pub fn get_cached_slice(&self, start: usize, end: usize) -> Option<(Vec<SearchResult>, usize)> {
        self.search_cache.as_ref().and_then(|cache| {
            if !cache.is_valid() {
                return None;
            }
            let total = cache.total;
            if total == 0 {
                return Some((Vec::new(), 0));
            }
            let results = cache.get_slice(&self.volumes, start, end);
            Some((results, total))
        })
    }

    pub fn invalidate_cache(&mut self) {
        self.search_cache = None;
    }
}

impl VolumeMonitor {
    pub fn new(drive_letter: String, include_hidden_files: bool, include_system_files: bool) -> Self {
        Self {
            drive_letter,
            files: Vec::new(),
            include_hidden_files,
            include_system_files,
        }
    }

    pub fn scan(&mut self) -> Result<usize> {
        self.files.clear();

        let path = if self.drive_letter.ends_with('\\') {
            self.drive_letter.clone()
        } else {
            format!("{}\\" , self.drive_letter)
        };

        let walker = walkdir::WalkDir::new(&path)
            .max_depth(10)
            .follow_links(self.include_hidden_files)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();

                if name.eq_ignore_ascii_case("$Recycle.Bin") {
                    return false;
                }

                if !self.include_hidden_files && name.starts_with('.') {
                    return false;
                }

                true
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

            let modified_ts = modified
                .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64)
                .unwrap_or_else(|| chrono::Utc::now().timestamp());

            let name = entry.file_name().to_string_lossy().to_string();
            let path_str = entry.path().to_string_lossy().to_string();

            let result = SearchResult {
                file_id: count as u64,
                name: name.into(),
                path: path_str.into(),
                size,
                modified_time: modified_ts,
                is_directory: is_dir,
            };

            self.files.push(result);
            count += 1;
        }

        Ok(count)
    }

    pub fn scan_with_progress_callback(&mut self, handle: &tauri::AppHandle) -> Result<usize> {
        self.files.clear();

        let path = if self.drive_letter.ends_with('\\') {
            self.drive_letter.clone()
        } else {
            format!("{}\\" , self.drive_letter)
        };

        let walker = walkdir::WalkDir::new(&path)
            .max_depth(10)
            .follow_links(self.include_hidden_files)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();

                if name.eq_ignore_ascii_case("$Recycle.Bin") {
                    return false;
                }

                if !self.include_hidden_files && name.starts_with('.') {
                    return false;
                }

                true
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

            let modified_ts = modified
                .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64)
                .unwrap_or_else(|| chrono::Utc::now().timestamp());

            let name = entry.file_name().to_string_lossy().to_string();
            let path_str = entry.path().to_string_lossy().to_string();

            let result = SearchResult {
                file_id: count as u64,
                name: name.into(),
                path: path_str.into(),
                size,
                modified_time: modified_ts,
                is_directory: is_dir,
            };

            self.files.push(result);
            count += 1;

            if count > 0 && count % 20000 == 0 {
                let _ = handle.emit(
                    "scan-progress",
                    serde_json::json!({"volume": self.drive_letter, "count": count}),
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

    pub fn remove_file(&mut self, file_path: &str) {
        self.files.retain(|f| f.path.as_ref() != file_path);
    }

    fn search(&self, query: &str) -> Vec<SearchResult> {
        let query_lower = query.to_lowercase();

        self.files
            .iter()
            .filter(|f| {
                let name_lower = f.name_lower();
                let path_lower = f.path_lower();
                name_lower.contains(&query_lower)
                    || path_lower.contains(&query_lower)
            })
            .cloned()
            .collect()
    }
}