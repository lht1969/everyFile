use crate::error::Result;
use crate::search::{SearchOptions, SearchResult, SortBy, SortDirection};
use rayon::prelude::*;
use std::collections::HashMap;
use std::collections::HashSet;
use std::time::{Duration, Instant};
use tauri::Emitter;

#[cfg(windows)]
use std::os::windows::fs::MetadataExt;

const CACHE_TTL_SECS: u64 = 3600;

pub struct IncrementalResult {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub total: usize,
    /// old_index → Some(new_index) for surviving files, None for removed
    pub index_map: Vec<Option<usize>>,
    /// indices (in rebuilt files Vec) of newly added files to check against query
    pub new_file_indices: Vec<usize>,
}

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
    query: String,
    files_only: bool,
    directories_only: bool,
    pub total: usize,
    pub created_at: Instant,
    pub matched: Vec<(u8, usize)>,
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
        vol_names: &[String],
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
            *slot = Some(build_sort_permutation(matched, volumes, vol_names, sort_by));
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
            let vol_name = &vol_names[*vol as usize];
            volumes.get(vol_name).and_then(|m| m.files.get(*file_idx)).cloned()
        }).collect()
    }
}

fn build_sort_permutation(matched: &[(u8, usize)], volumes: &HashMap<String, VolumeMonitor>, vol_names: &[String], sort_by: SortBy) -> Vec<usize> {
    let n = matched.len();
    if n == 0 {
        return Vec::new();
    }
    let mut v: Vec<usize> = (0..n).collect();
    match sort_by {
        SortBy::Name | SortBy::Score => {
            let keys: Vec<&str> = matched.iter()
                .map(|(vol, idx)| &*volumes[&vol_names[*vol as usize]].files[*idx].name)
                .collect();
            v.par_sort_unstable_by(|&a, &b| keys[a].cmp(keys[b]));
        }
        SortBy::Path => {
            let keys: Vec<&str> = matched.iter()
                .map(|(vol, idx)| &*volumes[&vol_names[*vol as usize]].files[*idx].path)
                .collect();
            v.par_sort_unstable_by(|&a, &b| keys[a].cmp(keys[b]));
        }
        SortBy::Size => {
            v.par_sort_unstable_by_key(|&i| volumes[&vol_names[matched[i].0 as usize]].files[matched[i].1].size);
        }
        SortBy::ModifiedTime => {
            v.par_sort_unstable_by_key(|&i| volumes[&vol_names[matched[i].0 as usize]].files[matched[i].1].modified_time);
        }
    }
    v
}

pub struct VolumeManager {
    volumes: HashMap<String, VolumeMonitor>,
    volume_index: HashMap<String, u8>,
    vol_names: Vec<String>,
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
            volume_index: HashMap::new(),
            vol_names: Vec::new(),
            search_cache: None,
        }
    }

    pub fn add_volume(&mut self, drive_letter: &str, _is_admin: bool, include_hidden_files: bool, include_system_files: bool) -> Result<()> {
        let idx = self.vol_names.len() as u8;
        self.volume_index.insert(drive_letter.to_string(), idx);
        self.vol_names.push(drive_letter.to_string());
        let monitor = VolumeMonitor::new(drive_letter.to_string(), include_hidden_files, include_system_files);
        self.volumes.insert(drive_letter.to_string(), monitor);
        Ok(())
    }

    pub fn remove_volume(&mut self, drive_letter: &str) {
        self.volumes.remove(drive_letter);
        if let Some(idx) = self.volume_index.remove(drive_letter) {
            self.vol_names[idx as usize].clear();
        }
        self.search_cache = None;
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

    pub fn search_with_options(&mut self, query: &str, options: &SearchOptions) -> (Vec<SearchResult>, usize) {
        let t0 = Instant::now();

        let mut matched: Vec<(u8, usize)> = Vec::new();
        let is_empty_query = query.trim().is_empty();
        let parsed_query = if is_empty_query {
            None
        } else {
            Some(crate::search::query::SearchQuery::parse(query))
        };
        let query_controls_dir = parsed_query.as_ref().map_or(false, |q| q.path_filter_dir_only);

        for (vol_key, monitor) in &self.volumes {
            let vol_idx = self.volume_index[vol_key];
            if is_empty_query {
                for (idx, file) in monitor.files.iter().enumerate() {
                    if options.files_only && file.is_directory { continue; }
                    if options.directories_only && !file.is_directory { continue; }
                    matched.push((vol_idx, idx));
                }
            } else {
                let pq = parsed_query.as_ref().unwrap();
                for (idx, file) in monitor.files.iter().enumerate() {
                    if !crate::search::query::SearchQuery::matches(pq, file) { continue; }
                    if !query_controls_dir && options.files_only && file.is_directory { continue; }
                    if options.directories_only && !file.is_directory { continue; }
                    matched.push((vol_idx, idx));
                }
            }
        }

        let total = matched.len();
        log::info!("search_with_options: matched {} files, {:?}", total, t0.elapsed());

        // 预计算当前排序字段的排列，避免首次请求时的 2s 延迟
        let default_perm = build_sort_permutation(&matched, &self.volumes, &self.vol_names, options.sort_by);

        let first_batch: Vec<SearchResult> = matched.iter().take(50)
            .filter_map(|(vol, idx)| {
                let vol_name = &self.vol_names[*vol as usize];
                self.volumes.get(vol_name).and_then(|m| m.files.get(*idx)).cloned()
            })
            .collect();

        self.search_cache = Some(SearchCache {
            query: query.to_string(),
            files_only: options.files_only,
            directories_only: options.directories_only,
            matched,
            total,
            created_at: Instant::now(),
            sorted_by_name: if options.sort_by == SortBy::Name || options.sort_by == SortBy::Score { Some(default_perm.clone()) } else { None },
            sorted_by_path: if options.sort_by == SortBy::Path { Some(default_perm.clone()) } else { None },
            sorted_by_size: if options.sort_by == SortBy::Size { Some(default_perm.clone()) } else { None },
            sorted_by_modified: if options.sort_by == SortBy::ModifiedTime { Some(default_perm) } else { None },
        });
        log::info!("search_with_options total: {:?}", t0.elapsed());

        (first_batch, total)
    }

    pub fn scan_all_with_progress(&mut self, handle: &tauri::AppHandle) -> Result<usize> {
        let mut total = 0;
        for (drive_letter, monitor) in self.volumes.iter_mut() {
            let count = monitor.scan_with_progress_callback(handle)?;
            log::info!("Scanned volume {}: {} files", drive_letter, count);
            total += count;
            let _ = handle.emit("scan-complete", serde_json::json!({"volume": drive_letter, "count": count}));
        }
        self.search_cache = None;
        Ok(total)
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
        let results = cache.get_sorted_slice(&self.volumes, &self.vol_names, sort_by, sort_direction, start, end);
        Some((results, total))
    }

    pub fn apply_incremental(&mut self, drive_letter: &str, result: &IncrementalResult) -> usize {
        // 分离借用：先获取 volume files 引用和索引，避免与 search_cache 的可变借用冲突
        let volume_files: Option<&[SearchResult]> = self.volumes.get(drive_letter).map(|v| v.files());
        let vol_idx = self.volume_index.get(drive_letter).copied();

        let cache = match self.search_cache.as_mut() {
            Some(c) => c,
            None => return 0,
        };

        let vol_idx = match vol_idx {
            Some(i) => i,
            None => return 0,
        };

        let mut new_matched: Vec<(u8, usize)> = Vec::with_capacity(cache.matched.len());
        let mut removed_count = 0usize;

        for (vol, idx) in cache.matched.drain(..) {
            if vol != vol_idx {
                new_matched.push((vol, idx));
            } else if idx < result.index_map.len() {
                if let Some(new_idx) = result.index_map[idx] {
                    new_matched.push((vol, new_idx));
                } else {
                    removed_count += 1;
                }
            } else {
                removed_count += 1;
            }
        }

        // Add new files that match the current search query
        let added_count_before = new_matched.len();
        if !result.new_file_indices.is_empty() {
            if let Some(files) = volume_files {
                let query = if cache.query.trim().is_empty() {
                    None
                } else {
                    Some(crate::search::query::SearchQuery::parse(&cache.query))
                };

                for &new_idx in &result.new_file_indices {
                    if new_idx >= files.len() { continue; }
                    let file = &files[new_idx];

                    if let Some(ref q) = query {
                        if !crate::search::query::SearchQuery::matches(q, file) { continue; }
                    }
                    if cache.files_only && file.is_directory { continue; }
                    if cache.directories_only && !file.is_directory { continue; }

                    new_matched.push((vol_idx, new_idx));
                }
            }
        }
        let added_count = new_matched.len() - added_count_before;

        cache.matched = new_matched;
        cache.total = cache.matched.len();

        // 只有新增或删除时才需要重建排列（修改不影响排序顺序）
        if removed_count > 0 || added_count > 0 {
            cache.sorted_by_name = None;
            cache.sorted_by_path = None;
            cache.sorted_by_size = None;
            cache.sorted_by_modified = None;
        }

        cache.total
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

    pub fn files(&self) -> &[SearchResult] {
        &self.files
    }

    /// Build a walkdir walker configured for this volume's settings.
    fn build_walker(&self) -> walkdir::WalkDir {
        let path = if self.drive_letter.ends_with('\\') {
            self.drive_letter.clone()
        } else {
            format!("{}\\" , self.drive_letter)
        };

        walkdir::WalkDir::new(&path)
            .follow_links(false)
    }

    /// Process a walker iterator, pushing results into self.files.
    /// Returns the count of files scanned.
    fn process_walker(
        &mut self,
        walker: walkdir::WalkDir,
        mut on_progress: Option<&mut dyn FnMut(usize)>,
    ) -> Result<usize> {
        let start_id = self.files.len() as u64;
        let mut count = 0usize;
        let include_hidden = self.include_hidden_files;
        let include_system = self.include_system_files;

        for entry in walker
            .into_iter()
            .filter_entry(move |e| {
                let name = e.file_name().to_string_lossy();
                if name.eq_ignore_ascii_case("$Recycle.Bin") { return false; }
                if !include_system && name.eq_ignore_ascii_case("System Volume Information") { return false; }
                if !include_hidden && name.starts_with('.') { return false; }
                true
            })
            .filter_map(|e| e.ok())
        {
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
                file_id: start_id + count as u64,
                name: name.into(),
                path: path_str.into(),
                size,
                modified_time: modified_ts,
                is_directory: is_dir,
            };

            self.files.push(result);
            count += 1;

            if count % 5000 == 0 {
                if let Some(ref mut cb) = on_progress {
                    cb(count);
                }
            }
        }

        Ok(count)
    }

    pub fn scan(&mut self) -> Result<usize> {
        self.files.clear();
        let walker = self.build_walker();
        self.process_walker(walker, None)
    }

    pub fn scan_with_progress_callback(&mut self, handle: &tauri::AppHandle) -> Result<usize> {
        self.files.clear();
        let walker = self.build_walker();
        let drive_letter = self.drive_letter.clone();
        let mut on_progress = |count: usize| {
            let _ = handle.emit(
                "scan-progress",
                serde_json::json!({"volume": drive_letter, "count": count}),
            );
        };
        self.process_walker(walker, Some(&mut on_progress))
    }

    pub fn scan_incremental(&mut self, handle: &tauri::AppHandle) -> Result<IncrementalResult> {
        let include_hidden = self.include_hidden_files;
        let include_system = self.include_system_files;

        // Build path→index map from existing files (不预分配，让 HashMap 自然增长)
        let mut path_map: HashMap<String, usize> = HashMap::new();
        for (i, f) in self.files.iter().enumerate() {
            path_map.insert(f.path.to_string(), i);
        }

        let mut visited: Vec<bool> = vec![false; self.files.len()];
        let mut added_paths: HashSet<String> = HashSet::new();
        let mut added = 0usize;
        let mut updated = 0usize;
        let drive_letter = self.drive_letter.clone();

        let walker = self.build_walker();
        for entry in walker
            .into_iter()
            .filter_entry(move |e| {
                let name = e.file_name().to_string_lossy();
                if name.eq_ignore_ascii_case("$Recycle.Bin") { return false; }
                if !include_system && name.eq_ignore_ascii_case("System Volume Information") { return false; }
                if !include_hidden && name.starts_with('.') { return false; }
                true
            })
            .filter_map(|e| e.ok())
        {
            let metadata = entry.metadata().ok();
            if let Some(ref m) = metadata {
                if should_skip_by_attr(self.include_hidden_files, self.include_system_files, m) {
                    continue;
                }
            }
            let (size, is_dir, _created, modified, _accessed) = if let Some(ref m) = metadata {
                (m.len(), m.is_dir(), m.created().ok(), m.modified().ok(), m.accessed().ok())
            } else {
                (0, false, None, None, None)
            };

            let modified_ts = modified
                .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64)
                .unwrap_or_else(|| chrono::Utc::now().timestamp());

            let name = entry.file_name().to_string_lossy().to_string();
            let path_str = entry.path().to_string_lossy().to_string();

            if let Some(&idx) = path_map.get(&path_str) {
                // Existing file — check if modified
                visited[idx] = true;
                let existing = &self.files[idx];
                if existing.modified_time != modified_ts || existing.size != size {
                    self.files[idx] = SearchResult {
                        file_id: existing.file_id,
                        name: name.into(),
                        path: path_str.into(),
                        size,
                        modified_time: modified_ts,
                        is_directory: is_dir,
                    };
                    updated += 1;
                }
            } else {
                // New file
                let file_id = self.files.len() as u64;
                self.files.push(SearchResult {
                    file_id,
                    name: name.into(),
                    path: path_str.clone().into(),
                    size,
                    modified_time: modified_ts,
                    is_directory: is_dir,
                });
                added_paths.insert(path_str.clone());
                path_map.insert(path_str, self.files.len() - 1);
                visited.push(true);
                added += 1;
            }

            let total_processed = added + updated;
            if total_processed > 0 && total_processed % 5000 == 0 {
                let _ = handle.emit(
                    "scan-progress",
                    serde_json::json!({"volume": drive_letter, "count": total_processed}),
                );
            }
        }

        // Remove files that were not visited (deleted files)
        let old_len = self.files.len();
        let mut index_map = vec![None; old_len];
        let mut new_files = Vec::with_capacity(old_len);
        // 用移动替代克隆，避免为字符串分配新内存
        for (i, file) in self.files.drain(..).enumerate() {
            if i < visited.len() && visited[i] {
                index_map[i] = Some(new_files.len());
                new_files.push(file);
            }
        }
        let removed = old_len - new_files.len();
        self.files = new_files;

        // Reassign file_ids to eliminate gaps
        for (i, f) in self.files.iter_mut().enumerate() {
            f.file_id = i as u64;
        }

        // Find indices of newly added files for cache update
        let new_file_indices: Vec<usize> = self.files.iter()
            .enumerate()
            .filter(|(_, f)| added_paths.contains(f.path.as_ref()))
            .map(|(i, _)| i)
            .collect();

        log::info!(
            "Incremental scan {}: +{} ~{} -{} (total: {})",
            drive_letter, added, updated, removed, self.files.len()
        );

        Ok(IncrementalResult { added, updated, removed, total: self.files.len(), index_map, new_file_indices })
    }

    pub fn remove_file(&mut self, file_path: &str) {
        self.files.retain(|f| f.path.as_ref() != file_path);
    }
}