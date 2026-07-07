use crate::error::Result;
use crate::search::{SearchOptions, SearchResult, SortBy, SortDirection};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tauri::Emitter;

#[cfg(windows)]
use std::os::windows::fs::MetadataExt;

const CACHE_TTL_SECS: u64 = 3600;

/// Check if a file entry should be skipped based on hidden/system attribute settings.
/// Returns true if the entry should be excluded (skipped).
///
/// Uses `#[cfg(windows)]` internally — on non-Windows always returns false.
fn should_skip_by_attr(include_hidden: bool, include_system: bool, meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        let attrs = meta.file_attributes();
        if !include_hidden && (attrs & 0x2) != 0 {
            return true;
        }
        if !include_system && (attrs & 0x4) != 0 {
            return true;
        }
    }
    false
}

pub struct SearchCache {
    #[allow(dead_code)]
    query: String,
    #[allow(dead_code)]
    files_only: bool,
    #[allow(dead_code)]
    directories_only: bool,
    pub total: usize,
    pub created_at: Instant,
    pub matched: Vec<(String, usize)>,
    // Lazily-computed ascending permutation vectors into `matched`
    sorted_by_name: Option<Vec<usize>>,
    sorted_by_path: Option<Vec<usize>>,
    sorted_by_size: Option<Vec<usize>>,
    sorted_by_modified: Option<Vec<usize>>,
}

impl SearchCache {
    pub fn is_valid(&self) -> bool {
        self.created_at.elapsed() < Duration::from_secs(CACHE_TTL_SECS)
    }

    pub fn refresh(&mut self) {
        self.created_at = Instant::now();
    }

    pub fn get_sorted_slice(
        &mut self,
        volumes: &HashMap<String, VolumeMonitor>,
        sort_by: SortBy,
        sort_direction: SortDirection,
        start: usize,
        end: usize,
    ) -> Vec<SearchResult> {
        let matched = &self.matched;
        let slot = match sort_by {
            SortBy::Name => &mut self.sorted_by_name,
            SortBy::Path => &mut self.sorted_by_path,
            SortBy::Size => &mut self.sorted_by_size,
            SortBy::ModifiedTime => &mut self.sorted_by_modified,
            SortBy::Score => &mut self.sorted_by_name,
        };
        if slot.is_none() {
            let t0 = Instant::now();
            *slot = Some(build_sort_permutation(matched, volumes, sort_by));
            log::info!("build_sort_permutation({:?}): {:?}", sort_by, t0.elapsed());
        }
        let indices = slot.as_ref().unwrap();
        let n = indices.len();
        if n == 0 {
            return Vec::new();
        }

        let (range_start, range_end) = match sort_direction {
            SortDirection::Ascending => (start.min(n), end.min(n)),
            SortDirection::Descending => {
                let s = n.saturating_sub(end.max(start)).min(n);
                let e = n.saturating_sub(start);
                (s, e.max(s))
            }
        };

        if range_start >= range_end || range_start >= n {
            return Vec::new();
        }

        let iter: Box<dyn Iterator<Item = &usize>> = match sort_direction {
            SortDirection::Ascending => Box::new(indices[range_start..range_end].iter()),
            SortDirection::Descending => Box::new(indices[range_start..range_end].iter().rev()),
        };

        iter.filter_map(|idx| {
            let (vol, file_idx) = &matched[*idx];
            volumes.get(vol).and_then(|m| m.files.get(*file_idx)).cloned()
        }).collect()
    }
}

fn build_sort_permutation(matched: &[(String, usize)], volumes: &HashMap<String, VolumeMonitor>, sort_by: SortBy) -> Vec<usize> {
    let n = matched.len();
    if n == 0 {
        return Vec::new();
    }
    // Pre-fetch sort keys once to avoid HashMap lookups inside the comparator
    match sort_by {
        SortBy::Name | SortBy::Score => {
            let keys: Vec<&str> = matched.iter().map(|(vol, idx)| &*volumes[vol].files[*idx].name).collect();
            let mut v: Vec<usize> = (0..n).collect();
            v.sort_by(|&a, &b| keys[a].cmp(keys[b]));
            v
        }
        SortBy::Path => {
            let keys: Vec<&str> = matched.iter().map(|(vol, idx)| &*volumes[vol].files[*idx].path).collect();
            let mut v: Vec<usize> = (0..n).collect();
            v.sort_by(|&a, &b| keys[a].cmp(keys[b]));
            v
        }
        SortBy::Size => {
            let keys: Vec<u64> = matched.iter().map(|(vol, idx)| volumes[vol].files[*idx].size).collect();
            let mut v: Vec<usize> = (0..n).collect();
            v.sort_by(|&a, &b| keys[a].cmp(&keys[b]));
            v
        }
        SortBy::ModifiedTime => {
            let keys: Vec<i64> = matched.iter().map(|(vol, idx)| volumes[vol].files[*idx].modified_time).collect();
            let mut v: Vec<usize> = (0..n).collect();
            v.sort_by(|&a, &b| keys[a].cmp(&keys[b]));
            v
        }
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
        let mut vols: Vec<String> = self.volumes.keys().cloned().collect();
        vols.sort_by(|a, b| {
            let a_letter = a.trim_end_matches(':').to_uppercase();
            let b_letter = b.trim_end_matches(':').to_uppercase();
            a_letter.cmp(&b_letter)
        });
        vols
    }

    #[allow(dead_code)]
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

    #[allow(dead_code)]
    pub fn get_file_count(&self, drive_letter: &str) -> usize {
        self.volumes
            .get(drive_letter)
            .map(|v| v.files.len())
            .unwrap_or(0)
    }

    #[allow(dead_code)]
    pub fn get_all_files(&self) -> Vec<SearchResult> {
        self.volumes
            .values()
            .flat_map(|v| v.files.clone())
            .collect()
    }

    pub fn search_with_options(&mut self, query: &str, options: &SearchOptions) -> (Vec<SearchResult>, usize) {
        let t0 = Instant::now();

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
                let query_controls_dir = parsed_query.path_filter_dir_only;
                for (idx, file) in monitor.files.iter().enumerate() {
                    if !crate::search::query::SearchQuery::matches(&parsed_query, file) { continue; }
                    if !query_controls_dir && options.files_only && file.is_directory { continue; }
                    if options.directories_only && !file.is_directory { continue; }
                    matched.push((vol_key.clone(), idx));
                }
            }
        }

        let total = matched.len();
        log::info!("search_with_options: matched {} files, {:?}", total, t0.elapsed());

        // Don't sort matched here — all sorting is lazy via permutation vectors.
        // First batch is taken in insertion order for instant response.
        let first_batch: Vec<SearchResult> = matched.iter().take(50)
            .filter_map(|(vol, idx)| self.volumes.get(vol).and_then(|m| m.files.get(*idx)).cloned())
            .collect();

        self.search_cache = Some(SearchCache {
            query: query.to_string(),
            files_only: options.files_only,
            directories_only: options.directories_only,
            matched,
            total,
            created_at: Instant::now(),
            sorted_by_name: None,
            sorted_by_path: None,
            sorted_by_size: None,
            sorted_by_modified: None,
        });
        log::info!("search_with_options total: {:?}", t0.elapsed());

        (first_batch, total)
    }

    #[allow(dead_code)]
    fn compare_for_sort(&self, a: &SearchResult, b: &SearchResult, sort_by: &SortBy, sort_direction: &SortDirection) -> std::cmp::Ordering {
        let cmp = match sort_by {
            SortBy::Name => a.name.cmp(&b.name),
            SortBy::Path => a.path.cmp(&b.path),
            SortBy::Size => a.size.cmp(&b.size),
            SortBy::ModifiedTime => a.modified_time.cmp(&b.modified_time),
            SortBy::Score => std::cmp::Ordering::Equal,
        };
        match sort_direction {
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

    #[allow(dead_code)]
    pub fn start_listening_all(&mut self) -> Result<()> {
        log::info!("Started listening to all volumes");
        Ok(())
    }

    #[allow(dead_code)]
    pub fn stop_listening_all(&mut self) {
        log::info!("Stopped listening to all volumes");
    }

    #[allow(dead_code)]
    pub fn process_all_events(&mut self) -> Result<usize> {
        Ok(0)
    }

    pub fn remove_file(&mut self, file_path: &str) {
        for monitor in self.volumes.values_mut() {
            monitor.remove_file(file_path);
        }
        self.search_cache = None;
    }

    pub fn get_cached_slice(&mut self, sort_by: SortBy, sort_direction: SortDirection, start: usize, end: usize) -> Option<(Vec<SearchResult>, usize)> {
        let cache = self.search_cache.as_mut()?;
        if !cache.is_valid() {
            return None;
        }
        cache.refresh();
        let total = cache.total;
        if total == 0 {
            return Some((Vec::new(), 0));
        }
        let results = cache.get_sorted_slice(&self.volumes, sort_by, sort_direction, start, end);
        Some((results, total))
    }

    #[allow(dead_code)]
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

    /// Update settings without recreating the monitor or clearing its file list.
    pub fn update_settings(&mut self, include_hidden_files: bool, include_system_files: bool) {
        self.include_hidden_files = include_hidden_files;
        self.include_system_files = include_system_files;
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
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();

                if name.eq_ignore_ascii_case("$Recycle.Bin") {
                    return false;
                }

                if !self.include_system_files {
                    if name.eq_ignore_ascii_case("System Volume Information") {
                        return false;
                    }
                }

                if !self.include_hidden_files && name.starts_with('.') {
                    return false;
                }

                true
            });

        let mut count = 0;

        for entry in walker.filter_map(|e| e.ok()) {
            let metadata = entry.metadata().ok();
            if let Some(ref m) = metadata {
                if should_skip_by_attr(self.include_hidden_files, self.include_system_files, m) {
                    continue;
                }
            }
            let (size, is_dir, _created, modified, _accessed) = if let Some(ref m) = metadata {
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
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();

                if name.eq_ignore_ascii_case("$Recycle.Bin") {
                    return false;
                }

                if !self.include_system_files {
                    if name.eq_ignore_ascii_case("System Volume Information") {
                        return false;
                    }
                }

                if !self.include_hidden_files && name.starts_with('.') {
                    return false;
                }

                true
            });

        let mut count = 0;

        for entry in walker.filter_map(|e| e.ok()) {
            let metadata = entry.metadata().ok();
            if let Some(ref m) = metadata {
                if should_skip_by_attr(self.include_hidden_files, self.include_system_files, m) {
                    continue;
                }
            }
            let (size, is_dir, _created, modified, _accessed) = if let Some(ref m) = metadata {
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

    #[allow(dead_code)]
    pub fn scan_with_progress(
        &mut self,
        _callback: Option<Box<dyn FnMut(usize, &str) + Send>>,
    ) -> Result<usize> {
        self.scan()
    }

    #[allow(dead_code)]
    pub fn get_all_files(&self) -> Vec<SearchResult> {
        self.files.clone()
    }

    #[allow(dead_code)]
    pub fn clear_index(&mut self) {
        self.files.clear();
    }

    pub fn remove_file(&mut self, file_path: &str) {
        self.files.retain(|f| f.path.as_ref() != file_path);
    }

    #[allow(dead_code)]
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