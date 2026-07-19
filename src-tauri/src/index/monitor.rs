use crate::error::Result;
use crate::search::{FileEntry, SearchOptions, SearchResult, SortBy, SortDirection};
use crate::index::path_table::PathTable;
use compact_str::CompactString;
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

// LRU 容量扩展到 4，支持所有排序字段同时缓存
// 每个 perm Vec 内存：221万 × 4字节 ≈ 8.4MB，4 个共 33.6MB
// 收益：用户在 Name/Path/Size/ModifiedTime 之间反复切换时无需重排序（每次重排 ~1-3s）
const _MAX_SORT_PERMUTATIONS: usize = 4;

/// 从完整文件路径中分离出父目录路径
/// "X:\dir\file.txt" → "X:\dir"
/// "X:\file.txt" → "X:\"
/// "X:\dir\subdir" → "X:\dir"（subdir 是目录，但其父目录路径仍需计算）
fn parent_dir_of(full_path: &str) -> &str {
    if let Some(pos) = full_path.rfind('\\') {
        if pos <= 2 {
            // "X:\file" → "X:\"
            &full_path[..pos + 1]
        } else {
            // "X:\dir\file" → "X:\dir"
            &full_path[..pos]
        }
    } else {
        // 异常情况：没有反斜杠
        "X:\\"
    }
}

/// Base cache: stable data from full scan. Sort permutations only rebuilt during merge.
pub struct BaseCache {
    pub matched: Vec<(u8, u32)>,
    sorted_by_name: Option<Vec<u32>>,
    sorted_by_path: Option<Vec<u32>>,
    sorted_by_size: Option<Vec<u32>>,
    sorted_by_modified: Option<Vec<u32>>,
}

/// Delta cache: incremental changes. new_files is APPEND-ONLY (no swap_remove!).
pub struct DeltaCache {
    pub new_files: Vec<(u8, FileEntry)>,
    pub file_id_index: HashMap<u32, usize>,
    pub deleted_ids: HashSet<u32>,
    pub modified: HashMap<u32, (u8, FileEntry)>,
    pub matched: Vec<(u8, u32)>,
}

pub struct SearchCache {
    cache_key: u64,
    query: String,
    files_only: bool,
    directories_only: bool,
    pub created_at: Instant,
    pub base: BaseCache,
    pub delta: DeltaCache,
}

impl SearchCache {
    pub fn is_valid(&self) -> bool {
        self.created_at.elapsed() < Duration::from_secs(CACHE_TTL_SECS)
    }

    pub fn refresh(&mut self) {
        self.created_at = Instant::now();
    }

    fn total_matched(&self) -> usize {
        self.base.matched.len() + self.delta.matched.len()
    }

    /// Merge delta into base. Called periodically.
    pub fn merge_delta_to_base(&mut self, volumes: &mut HashMap<String, VolumeMonitor>, vol_names: &[String]) {
        if self.delta.new_files.is_empty() && self.delta.deleted_ids.is_empty() && self.delta.modified.is_empty() {
            return;
        }
        let t0 = Instant::now();

        // Apply modifications to base.files
        for (&fid, &(vol_idx, ref new_entry)) in &self.delta.modified {
            let vol_name = &vol_names[vol_idx as usize];
            if let Some(monitor) = volumes.get_mut(vol_name) {
                if let Some(fi) = monitor.fid_index.as_ref() {
                    if let Ok(pos) = fi.binary_search_by_key(&fid, |(id, _)| *id) {
                        let idx = fi[pos].1 as usize;
                        if idx < monitor.files.len() {
                            monitor.files[idx] = new_entry.clone();
                        }
                    }
                }
            }
        }

        // Mark deleted files in base.files
        for &fid in &self.delta.deleted_ids {
            for monitor in volumes.values_mut() {
                if let Some(fi) = monitor.fid_index.as_ref() {
                    if let Ok(pos) = fi.binary_search_by_key(&fid, |(id, _)| *id) {
                        let idx = fi[pos].1 as usize;
                        if idx < monitor.files.len() {
                            monitor.files[idx].path_id = PathTable::deleted_id();
                        }
                    }
                }
            }
        }

        // Append delta.new_files to base.files
        for (vol_idx, file_entry) in &self.delta.new_files {
            let vol_name = &vol_names[*vol_idx as usize];
            if let Some(monitor) = volumes.get_mut(vol_name) {
                monitor.files.push(file_entry.clone());
            }
        }

        // Rebuild fid_index for affected volumes and compact
        let mut affected_vols: HashSet<u8> = self.delta.new_files.iter().map(|(v, _)| *v).collect();
        for &fid in self.delta.modified.keys().chain(self.delta.deleted_ids.iter()) {
            for (idx, (_vn, monitor)) in volumes.iter().enumerate() {
                if let Some(fi) = monitor.fid_index.as_ref() {
                    if fi.binary_search_by_key(&fid, |(id, _)| *id).is_ok() {
                        affected_vols.insert(idx as u8);
                    }
                }
            }
        }
        for vol_idx in &affected_vols {
            let vol_name = &vol_names[*vol_idx as usize];
            if let Some(monitor) = volumes.get_mut(vol_name) {
                let mut fid_index: Vec<(u32, u32)> = Vec::with_capacity(monitor.files.len());
                for (i, f) in monitor.files.iter().enumerate() {
                    if !PathTable::is_deleted(f.path_id) {
                        fid_index.push((f.file_id, i as u32));
                    }
                }
                fid_index.sort_unstable_by_key(|(id, _)| *id);
                monitor.fid_index = Some(fid_index);
                monitor.compact_files();
            }
        }

        // Clear delta
        self.delta.new_files.clear();
        self.delta.file_id_index.clear();
        self.delta.deleted_ids.clear();
        self.delta.modified.clear();
        self.delta.matched.clear();

        // Rebuild base matched from scratch
        let mut matched = Vec::new();
        for (vol_name, monitor) in volumes.iter() {
            let vol_idx = vol_names.iter().position(|n| n == vol_name).unwrap() as u8;
            for (file_idx, file) in monitor.files.iter().enumerate() {
                if PathTable::is_deleted(file.path_id) { continue; }
                if self.files_only && file.is_directory { continue; }
                if self.directories_only && !file.is_directory { continue; }
                matched.push((vol_idx, file_idx as u32));
            }
        }
        self.base.matched = matched;

        // Invalidate base permutations (will be rebuilt lazily)
        self.base.sorted_by_name = None;
        self.base.sorted_by_path = None;
        self.base.sorted_by_size = None;
        self.base.sorted_by_modified = None;

        log::info!("merge_delta_to_base: done in {:?}, base={} entries", t0.elapsed(), self.base.matched.len());
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
        // Snapshot delta.new_files to avoid inconsistency if it's modified during this call
        let delta_files: Vec<(u8, FileEntry)> = self.delta.new_files.clone();
        let delta_deleted = self.delta.deleted_ids.clone();
        let delta_modified = self.delta.modified.clone();

        // Build base permutation if needed
        let base_needs_build = match sort_by {
            SortBy::Name | SortBy::Score => self.base.sorted_by_name.is_none(),
            SortBy::Path => self.base.sorted_by_path.is_none(),
            SortBy::Size => self.base.sorted_by_size.is_none(),
            SortBy::ModifiedTime => self.base.sorted_by_modified.is_none(),
        };
        if base_needs_build && !self.base.matched.is_empty() {
            let perm = build_sort_permutation(&self.base.matched, volumes, vol_names, sort_by);
            match sort_by {
                SortBy::Name | SortBy::Score => { self.base.sorted_by_name = Some(perm); }
                SortBy::Path => { self.base.sorted_by_path = Some(perm); }
                SortBy::Size => { self.base.sorted_by_size = Some(perm); }
                SortBy::ModifiedTime => { self.base.sorted_by_modified = Some(perm); }
            }
        }

        let base_perm = match sort_by {
            SortBy::Name | SortBy::Score => self.base.sorted_by_name.as_ref(),
            SortBy::Path => self.base.sorted_by_path.as_ref(),
            SortBy::Size => self.base.sorted_by_size.as_ref(),
            SortBy::ModifiedTime => self.base.sorted_by_modified.as_ref(),
        };

        // Build filtered base indices (skip deleted_ids)
        let active_base: Vec<u32> = if let Some(perm) = base_perm {
            if delta_deleted.is_empty() {
                perm.clone()
            } else {
                perm.iter().copied().filter(|idx| {
                    let (vol, file_idx) = &self.base.matched[*idx as usize];
                    let vol_name = &vol_names[*vol as usize];
                    if let Some(m) = volumes.get(vol_name) {
                        if let Some(f) = m.files.get(*file_idx as usize) {
                            return !delta_deleted.contains(&f.file_id);
                        }
                    }
                    true
                }).collect()
            }
        } else {
            Vec::new()
        };

        // Build sorted delta entries directly from delta_files
        // No index indirection - sort (index, key) tuples directly
        let query = if self.query.trim().is_empty() { None } else {
            Some(crate::search::query::SearchQuery::parse(&self.query))
        };
        let needs_path = query.as_ref().map_or(false, |q| q.path_filter.is_some());
        let df_len = delta_files.len();
        let mut delta_entries: Vec<(usize, &str, u64, i32, CompactString)> = Vec::new();
        for i in 0..df_len {
            let (_vol, f) = &delta_files[i];
            if delta_deleted.contains(&f.file_id) { continue; }
            if let Some(ref q) = query {
                let vol_name = &vol_names[*_vol as usize];
                if let Some(m) = volumes.get(vol_name) {
                    let full_path = if needs_path {
                        m.path_table.resolve_file_path(f.path_id, &f.name)
                    } else { CompactString::new("") };
                    if !crate::search::query::SearchQuery::matches_entry(q, f, &full_path) { continue; }
                }
            }
            if self.files_only && f.is_directory { continue; }
            if self.directories_only && !f.is_directory { continue; }
            let path = if needs_path {
                let vol_name = &vol_names[*_vol as usize];
                volumes.get(vol_name).map(|m| m.path_table.resolve_file_path(f.path_id, &f.name)).unwrap_or_default()
            } else { CompactString::new("") };
            delta_entries.push((i, f.name.as_str(), f.size, f.modified_time, path));
        }

        match sort_by {
            SortBy::Name | SortBy::Score => {
                delta_entries.sort_by(|a, b| a.1.cmp(b.1).then(a.0.cmp(&b.0)));
            }
            SortBy::Path => {
                delta_entries.sort_by(|a, b| a.4.cmp(&b.4).then(a.1.cmp(b.1)).then(a.0.cmp(&b.0)));
            }
            SortBy::Size => {
                delta_entries.sort_by(|a, b| a.2.cmp(&b.2).then(a.0.cmp(&b.0)));
            }
            SortBy::ModifiedTime => {
                delta_entries.sort_by(|a, b| a.3.cmp(&b.3).then(a.0.cmp(&b.0)));
            }
        }

        let base_n = active_base.len();
        let delta_n = delta_entries.len();
        let total_n = base_n + delta_n;
        if total_n == 0 { return Vec::new(); }

        let (eff_start, eff_end) = match sort_direction {
            SortDirection::Ascending => (start.min(total_n), end.min(total_n)),
            SortDirection::Descending => {
                let s = total_n.saturating_sub(end.max(start)).min(total_n);
                let e = total_n.saturating_sub(start);
                (s, e.max(s))
            }
        };
        if eff_start >= eff_end || eff_start >= total_n { return Vec::new(); }

        // Two-pointer merge
        match sort_direction {
            SortDirection::Ascending => {
                let mut results = Vec::with_capacity(eff_end - eff_start);
                let (mut bi, mut di, mut pos) = (0usize, 0usize, 0usize);
                while pos < eff_end && (bi < base_n || di < delta_n) {
                    let pick_base = if di >= delta_n { true }
                    else if bi >= base_n { false }
                    else {
                        let a = &self.base.matched[active_base[bi] as usize];
                        let vol_a = &vol_names[a.0 as usize];
                        let Some(ma) = volumes.get(vol_a) else {
                            return results;
                        };
                        let fa = &ma.files[a.1 as usize];
                        let (sort_name, sort_size, sort_modified, sort_path) = if let Some(&(mv, ref mf)) = delta_modified.get(&fa.file_id) {
                            let mvol_name = &vol_names[mv as usize];
                            match volumes.get(mvol_name) {
                                None => (fa.name.as_str(), fa.size, fa.modified_time, ma.path_table.resolve_file_path(fa.path_id, &fa.name)),
                                Some(mb) => (mf.name.as_str(), mf.size, mf.modified_time, mb.path_table.resolve_file_path(mf.path_id, &mf.name)),
                            }
                        } else {
                            (fa.name.as_str(), fa.size, fa.modified_time, ma.path_table.resolve_file_path(fa.path_id, &fa.name))
                        };
                        let b = &delta_files[delta_entries[di].0].1;
                        let vol_b_name = &vol_names[delta_files[delta_entries[di].0].0 as usize];
                        let Some(mb) = volumes.get(vol_b_name) else {
                            return results;
                        };
                        let pb = mb.path_table.resolve_file_path(b.path_id, &b.name);
                        let ord = match sort_by {
                            SortBy::Name | SortBy::Score => sort_name.cmp(&b.name),
                            SortBy::Path => sort_path.cmp(&pb),
                            SortBy::Size => sort_size.cmp(&b.size),
                            SortBy::ModifiedTime => sort_modified.cmp(&b.modified_time),
                        };
                        ord != std::cmp::Ordering::Greater
                    };
                    if pos >= eff_start {
                        if pick_base {
                            let &(vol, file_idx) = &self.base.matched[active_base[bi] as usize];
                            let vol_name = &vol_names[vol as usize];
                            if let Some(m) = volumes.get(vol_name) {
                                if let Some(f) = m.files.get(file_idx as usize) {
                                    if let Some(&(mv, ref mf)) = delta_modified.get(&f.file_id) {
                                        let mvol_name = &vol_names[mv as usize];
                                        if let Some(mm) = volumes.get(mvol_name) {
                                            let full_path = mm.path_table.resolve_file_path(mf.path_id, &mf.name);
                                            results.push(mf.to_search_result(full_path));
                                        }
                                    } else {
                                        let full_path = m.path_table.resolve_file_path(f.path_id, &f.name);
                                        results.push(f.to_search_result(full_path));
                                    }
                                }
                            }
                        } else {
                            let de = &delta_entries[di];
                            let &(vol, ref f) = &delta_files[de.0];
                            let vol_name = &vol_names[vol as usize];
                            if let Some(m) = volumes.get(vol_name) {
                                let full_path = m.path_table.resolve_file_path(f.path_id, &f.name);
                                results.push(f.to_search_result(full_path));
                            }
                        }
                    }
                    if pick_base { bi += 1; } else { di += 1; }
                    pos += 1;
                }
                results
            }
            SortDirection::Descending => {
                let mut results = Vec::with_capacity(eff_end - eff_start);
                let (mut bi, mut di, mut pos) = (base_n, delta_n, total_n);
                while pos > eff_start && (bi > 0 || di > 0) {
                    pos -= 1;
                    let pick_base = if di == 0 { true }
                    else if bi == 0 { false }
                    else {
                        let a = &self.base.matched[active_base[bi - 1] as usize];
                        let vol_a = &vol_names[a.0 as usize];
                        let Some(ma) = volumes.get(vol_a) else {
                            return results;
                        };
                        let fa = &ma.files[a.1 as usize];
                        let (sort_name, sort_size, sort_modified, sort_path) = if let Some(&(mv, ref mf)) = delta_modified.get(&fa.file_id) {
                            let mvol_name = &vol_names[mv as usize];
                            match volumes.get(mvol_name) {
                                None => (fa.name.as_str(), fa.size, fa.modified_time, ma.path_table.resolve_file_path(fa.path_id, &fa.name)),
                                Some(mb) => (mf.name.as_str(), mf.size, mf.modified_time, mb.path_table.resolve_file_path(mf.path_id, &mf.name)),
                            }
                        } else {
                            (fa.name.as_str(), fa.size, fa.modified_time, ma.path_table.resolve_file_path(fa.path_id, &fa.name))
                        };
                        let b = &delta_files[delta_entries[di - 1].0].1;
                        let vol_b_name = &vol_names[delta_files[delta_entries[di - 1].0].0 as usize];
                        let Some(mb) = volumes.get(vol_b_name) else {
                            return results;
                        };
                        let pb = mb.path_table.resolve_file_path(b.path_id, &b.name);
                        let ord = match sort_by {
                            SortBy::Name | SortBy::Score => sort_name.cmp(&b.name),
                            SortBy::Path => sort_path.cmp(&pb),
                            SortBy::Size => sort_size.cmp(&b.size),
                            SortBy::ModifiedTime => sort_modified.cmp(&b.modified_time),
                        };
                        ord != std::cmp::Ordering::Less
                    };
                    if pos < eff_end {
                        if pick_base {
                            bi -= 1;
                            let &(vol, file_idx) = &self.base.matched[active_base[bi] as usize];
                            let vol_name = &vol_names[vol as usize];
                            if let Some(m) = volumes.get(vol_name) {
                                if let Some(f) = m.files.get(file_idx as usize) {
                                    if let Some(&(mv, ref mf)) = delta_modified.get(&f.file_id) {
                                        let mvol_name = &vol_names[mv as usize];
                                        if let Some(mm) = volumes.get(mvol_name) {
                                            let full_path = mm.path_table.resolve_file_path(mf.path_id, &mf.name);
                                            results.push(mf.to_search_result(full_path));
                                        }
                                    } else {
                                        let full_path = m.path_table.resolve_file_path(f.path_id, &f.name);
                                        results.push(f.to_search_result(full_path));
                                    }
                                }
                            }
                        } else {
                            di -= 1;
                            let de = &delta_entries[di];
                            let &(vol, ref f) = &delta_files[de.0];
                            let vol_name = &vol_names[vol as usize];
                            if let Some(m) = volumes.get(vol_name) {
                                let full_path = m.path_table.resolve_file_path(f.path_id, &f.name);
                                results.push(f.to_search_result(full_path));
                            }
                        }
                    } else {
                        if pick_base { bi -= 1; } else { di -= 1; }
                    }
                }
                results
            }
        }
    }
}

fn build_sort_permutation(matched: &[(u8, u32)], volumes: &HashMap<String, VolumeMonitor>, vol_names: &[String], sort_by: SortBy) -> Vec<u32> {
    let n = matched.len();
    if n == 0 {
        return Vec::new();
    }
    // 预构建 vol_idx → files 切片和 path_table 的数组
    // 避免 221万次 HashMap 查找（volumes[&vol_names[*vol as usize]]）
    // HashMap 查找需要哈希计算 + 比较，而数组索引是 O(1)
    let vol_files: Vec<&[FileEntry]> = (0..vol_names.len())
        .map(|i| volumes.get(&vol_names[i]).map(|v| v.files.as_slice()).unwrap_or(&[]))
        .collect();
    let vol_path_tables: Vec<Option<&PathTable>> = (0..vol_names.len())
        .map(|i| volumes.get(&vol_names[i]).map(|v| &v.path_table))
        .collect();

    let mut v: Vec<u32> = (0..n as u32).collect();
    // Filter out entries referencing empty volumes (volume was cleared/removed)
    v.retain(|&i| {
        let (vol, _idx) = matched[i as usize];
        !vol_files[vol as usize].is_empty()
    });
    // 所有排序分支都添加原始索引作为二级排序键
    // 原因：par_sort_unstable_by 是不稳定排序，相同 key 的元素顺序不确定
    // 当 LRU 淘汰排序缓存后重新构建时，相同 key 的文件顺序会变化，导致"多次排序后结果错乱"
    // 用原始索引（即 matched 中的顺序）作为 tiebreaker，保证相同 key 时顺序确定性
    match sort_by {
        SortBy::Name | SortBy::Score => {
            // 通过预构建的 vol_files 数组直接索引，避免 HashMap 查找
            // 关键优化：使用 par_iter().map() 并行收集 keys，
            // 把串行的 221万次指针解引用 + 字符串 slice 操作分摊到多核
            let keys: Vec<&str> = matched.par_iter()
                .map(|(vol, idx)| vol_files[*vol as usize][*idx as usize].name.as_str())
                .collect();
            v.par_sort_unstable_by(|&a, &b| keys[a as usize].cmp(keys[b as usize]).then(a.cmp(&b)));
        }
        SortBy::Path => {
            // FileEntry.path_id 指向父目录，需用 ordinal + name 排序
            // 关键优化：用预计算的 ordinal (u32) 替代完整路径字符串比较
            // 221万次比较从 O(strlen) 字符串 cmp 降至 O(1) u32 cmp
            // 预期 8 核 CPU 上首次 Path 排序从 ~2s 降至 ~500ms
            let keys: Vec<(u32, &str)> = matched.par_iter()
                .map(|(vol, idx)| {
                    let pt = vol_path_tables[*vol as usize].unwrap();
                    let f = &vol_files[*vol as usize][*idx as usize];
                    (pt.get_ordinal(f.path_id), f.name.as_str())
                })
                .collect();
            v.par_sort_unstable_by(|&a, &b| {
                keys[a as usize].0.cmp(&keys[b as usize].0)
                    .then(keys[a as usize].1.cmp(keys[b as usize].1))
                    .then(a.cmp(&b))
            });
        }
        SortBy::Size => {
            let keys: Vec<u64> = matched.par_iter()
                .map(|(vol, idx)| vol_files[*vol as usize][*idx as usize].size)
                .collect();
            v.par_sort_unstable_by(|&a, &b| keys[a as usize].cmp(&keys[b as usize]).then(a.cmp(&b)));
        }
        SortBy::ModifiedTime => {
            // FileEntry 的 modified_time 是 i32（SearchResult 是 i64）
            // 直接以 i32 排序结果与 i64 一致，无需扩展
            let keys: Vec<i32> = matched.par_iter()
                .map(|(vol, idx)| vol_files[*vol as usize][*idx as usize].modified_time)
                .collect();
            v.par_sort_unstable_by(|&a, &b| keys[a as usize].cmp(&keys[b as usize]).then(a.cmp(&b)));
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
    /// 内部紧凑存储：使用 FileEntry 而非 SearchResult
    /// FileEntry 用 path_id 替代完整路径字符串，大幅节省内存
    files: Vec<FileEntry>,
    /// 路径前缀压缩表：path_id → 完整路径
    /// 所有文件的路径通过此表按需解析，避免冗余存储
    path_table: PathTable,
    include_hidden_files: bool,
    include_system_files: bool,
    /// fid_index: (file_id, files_vec_index)
    /// 使用 (u32, u32) 而非 (u64, u32) 以节省内存：
    /// MFT record number 在 221 万文件下完全可用 u32 表示
    /// 每项从 16 字节降至 8 字节，节省 50% 内存
    pub fid_index: Option<Vec<(u32, u32)>>,
    pub use_usn: bool,
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
        self.search_cache = None;
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

        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        query.hash(&mut hasher);
        options.files_only.hash(&mut hasher);
        options.directories_only.hash(&mut hasher);
        let new_cache_key = hasher.finish();

        // Check cache reuse
        if let Some(old) = self.search_cache.as_mut() {
            if old.is_valid() && old.cache_key == new_cache_key {
                log::info!("search_with_options: reusing cache (key={})", new_cache_key);
                old.refresh();
                let total = old.total_matched();
                let first_batch = old.get_sorted_slice(&self.volumes, &self.vol_names, options.sort_by, options.sort_direction, 0, 50);
                log::info!("search_with_options total: {:?}", t0.elapsed());
                return (first_batch, total);
            }
        }

        // New search: build base matched from all volumes
        self.search_cache = None;
        let total_files: usize = self.volumes.values().map(|v| v.files.len()).sum();
        let matched_lock = std::sync::Mutex::new(Vec::with_capacity(total_files / 4));
        let is_empty_query = query.trim().is_empty();
        let parsed_query = if is_empty_query { None } else { Some(crate::search::query::SearchQuery::parse(query)) };
        let query_controls_dir = parsed_query.as_ref().map_or(false, |q| q.path_filter_dir_only);
        let needs_path = parsed_query.as_ref().map_or(false, |q| q.path_filter.is_some());
        let files_only = options.files_only;
        let directories_only = options.directories_only;

        self.volumes.par_iter().for_each(|(vol_key, monitor)| {
            let vol_idx = self.volume_index[vol_key];
            if is_empty_query {
                let local: Vec<(u8, u32)> = monitor.files.par_iter().enumerate()
                    .filter_map(|(idx, file)| {
                        if files_only && file.is_directory { return None; }
                        if directories_only && !file.is_directory { return None; }
                        Some((vol_idx, idx as u32))
                    }).collect();
                matched_lock.lock().unwrap().extend(local);
            } else {
                let pq = parsed_query.as_ref().unwrap();
                let local: Vec<(u8, u32)> = monitor.files.par_iter().enumerate()
                    .filter_map(|(idx, file)| {
                        let full_path = if needs_path {
                            monitor.path_table.resolve_file_path(file.path_id, &file.name)
                        } else {
                            CompactString::new("")
                        };
                        if !crate::search::query::SearchQuery::matches_entry(pq, file, &full_path) { return None; }
                        if !query_controls_dir && files_only && file.is_directory { return None; }
                        if directories_only && !file.is_directory { return None; }
                        Some((vol_idx, idx as u32))
                    }).collect();
                matched_lock.lock().unwrap().extend(local);
            }
        });

        let all_matched = matched_lock.into_inner().unwrap();
        let total = all_matched.len();
        log::info!("search_with_options: matched {} files, {:?}", total, t0.elapsed());

        // Build all 4 base permutations
        let sn = Some(build_sort_permutation(&all_matched, &self.volumes, &self.vol_names, SortBy::Name));
        let sp = Some(build_sort_permutation(&all_matched, &self.volumes, &self.vol_names, SortBy::Path));
        let ss = Some(build_sort_permutation(&all_matched, &self.volumes, &self.vol_names, SortBy::Size));
        let sm = Some(build_sort_permutation(&all_matched, &self.volumes, &self.vol_names, SortBy::ModifiedTime));

        self.search_cache = Some(SearchCache {
            cache_key: new_cache_key,
            query: query.to_string(),
            files_only: options.files_only,
            directories_only: options.directories_only,
            created_at: Instant::now(),
            base: BaseCache {
                matched: all_matched,
                sorted_by_name: sn,
                sorted_by_path: sp,
                sorted_by_size: ss,
                sorted_by_modified: sm,
            },
            delta: DeltaCache {
                new_files: Vec::new(),
                file_id_index: HashMap::new(),
                deleted_ids: HashSet::new(),
                modified: HashMap::new(),
                matched: Vec::new(),
            },
        });

        // Use get_sorted_slice for first_batch (handles base+delta merge)
        let first_batch = self.search_cache.as_mut().unwrap().get_sorted_slice(
            &self.volumes, &self.vol_names, options.sort_by, options.sort_direction, 0, 50,
        );
        let total = self.search_cache.as_ref().unwrap().total_matched();
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

    /// Update settings on all live monitors.
    pub fn update_all_settings(&mut self, include_hidden_files: bool, include_system_files: bool) {
        for monitor in self.volumes.values_mut() {
            monitor.update_settings(include_hidden_files, include_system_files);
        }
    }

    pub fn get_cached_slice(&mut self, sort_by: SortBy, sort_direction: SortDirection, start: usize, end: usize) -> Option<(Vec<SearchResult>, usize)> {
        let cache = self.search_cache.as_mut()?;
        if !cache.is_valid() {
            return None;
        }
        cache.refresh();
        let total = cache.total_matched();
        if total == 0 {
            return Some((Vec::new(), 0));
        }
        let results = cache.get_sorted_slice(&self.volumes, &self.vol_names, sort_by, sort_direction, start, end);
        Some((results, total))
    }

    /// 应用全量 USN 扫描结果：替换卷的文件列表、路径表与 fid_index
    ///
    /// 签名变更：原本接收 Vec<SearchResult>，现在接收 Vec<FileEntry> 和 PathTable
    /// 调用方（usn_worker.rs）需同步适配
    pub fn apply_full_scan(
        &mut self,
        drive_letter: &str,
        mut files: Vec<FileEntry>,
        mut path_table: PathTable,
    ) {
        if let Some(monitor) = self.volumes.get_mut(drive_letter) {
            let mut fid_index: Vec<(u32, u32)> = Vec::with_capacity(files.len());
            for (i, f) in files.iter().enumerate() {
                fid_index.push((f.file_id, i as u32));
            }
            fid_index.sort_unstable_by_key(|(id, _)| *id);
            // 释放去重 HashMap 内存：扫描完成后不再需要批量去重
            // 对于 221万文件场景，path_to_id 存储 ~40万目录路径字符串，
            // 清理后释放约 40-100MB 内存
            path_table.clear_dedup_map();
            // 关键优化：为所有 path 分配字典序 ordinal，使 Path 排序用 O(1) 整数比较
            // 替代 O(strlen) 字符串比较，百万级 Path 排序从 ~2s 降至 ~500ms
            // 必须先 clear_dedup_map 再 compute_ordinals（clear_dedup_map 会 shrink_to_fit，释放借引用）
            path_table.compute_ordinals();
            // 压缩 files Vec，释放预留但未使用的容量
            files.shrink_to_fit();
            monitor.files = files;
            monitor.path_table = path_table;
            monitor.fid_index = Some(fid_index);
            monitor.use_usn = true;
        }
        self.search_cache = None;
    }

    /// 应用增量 USN 变更到卷
    ///
    /// 入参仍为 SearchResult（USN worker 的输出），内部转换为 FileEntry：
    /// - 通过 path_table.intern 注册路径得到 path_id
    /// - modified_time 从 i64 截断为 i32（FileEntry 内部存储）
    /// - 删除标记改为 path_id = PathTable::deleted_id()（u32::MAX）
    /// Apply incremental USN changes. base.files is NOT modified (no compaction).
    /// - Updates: modify base.files in-place (index stays valid)
    /// - Deletions: record in delta.deleted_ids
    /// - Additions: append to delta.new_files (append-only, no swap_remove!)
    pub fn apply_incremental_usn(
        &mut self,
        drive_letter: &str,
        added: Vec<SearchResult>,
        removed: Vec<u64>,
        updated: Vec<(u64, SearchResult)>,
    ) {
        let vol_idx = match self.volume_index.get(drive_letter).copied() {
            Some(i) => i,
            None => return,
        };
        let has_cache = self.search_cache.is_some();

        let monitor = match self.volumes.get_mut(drive_letter) {
            Some(m) => m,
            None => return,
        };
        if monitor.fid_index.is_none() {
            log::warn!("[USN] No fid_index for {}, skipping", drive_letter);
            return;
        }

        // Phase 1: Updates - store in delta.modified, NOT in base.files
        let fid_index = monitor.fid_index.as_ref().unwrap();
        for (fid, new_result) in &updated {
            let fid_u32 = *fid as u32;
            if fid_index.binary_search_by_key(&fid_u32, |(id, _)| *id).is_ok() {
                let parent_path = parent_dir_of(&new_result.path);
                let path_id = monitor.path_table.intern(parent_path);
                if new_result.is_directory {
                    monitor.path_table.intern(&new_result.path);
                }
                let modified_entry = FileEntry::new(
                    new_result.name.clone(), path_id, new_result.size,
                    new_result.modified_time, new_result.file_id, new_result.is_directory,
                );
                if let Some(cache) = self.search_cache.as_mut() {
                    cache.delta.modified.insert(fid_u32, (vol_idx, modified_entry));
                }
            }
        }

        // Phase 2: Deletions - record IDs only
        if has_cache {
            let cache = self.search_cache.as_mut().unwrap();
            for fid in &removed {
                let fid_u32 = *fid as u32;
                if fid_index.binary_search_by_key(&fid_u32, |(id, _)| *id).is_ok() {
                    cache.delta.deleted_ids.insert(fid_u32);
                } else if cache.delta.file_id_index.contains_key(&fid_u32) {
                    cache.delta.deleted_ids.insert(fid_u32);
                }
            }
        }

        // Phase 3: Additions - intern paths, collect entries
        let mut added_entries: Vec<(u8, FileEntry)> = Vec::new();
        {
            let monitor = self.volumes.get_mut(drive_letter).unwrap();
            for search_result in added {
                let parent_path = parent_dir_of(&search_result.path);
                let path_id = monitor.path_table.intern(parent_path);
                if search_result.is_directory {
                    monitor.path_table.intern(&search_result.path);
                }
                added_entries.push((vol_idx, FileEntry::new(
                    search_result.name, path_id, search_result.size,
                    search_result.modified_time, search_result.file_id, search_result.is_directory,
                )));
            }
        }

        // Phase 4: Update cache
        if has_cache {
            let cache = self.search_cache.as_mut().unwrap();
            // Add new files to delta (append-only)
            for entry in added_entries {
                let file_idx = cache.delta.new_files.len();
                // If this file_id was previously deleted or modified, clear the stale entries
                cache.delta.deleted_ids.remove(&entry.1.file_id);
                cache.delta.modified.remove(&entry.1.file_id);
                cache.delta.file_id_index.insert(entry.1.file_id, file_idx);
                cache.delta.new_files.push(entry);
            }
            // Rebuild delta.matched from delta.new_files
            let mut new_delta_matched: Vec<(u8, u32)> = Vec::new();
            for (i, (v, f)) in cache.delta.new_files.iter().enumerate() {
                if cache.delta.deleted_ids.contains(&f.file_id) { continue; }
                new_delta_matched.push((*v, i as u32));
            }
            cache.delta.matched = new_delta_matched;
            // Invalidate base permutations if base.files was modified
            if !cache.delta.modified.is_empty() {
                cache.base.sorted_by_name = None;
                cache.base.sorted_by_path = None;
                cache.base.sorted_by_size = None;
                cache.base.sorted_by_modified = None;
            }
        }
    }

    pub fn merge_if_needed(&mut self) {
        let should_merge = self.search_cache.as_ref().map_or(false, |c| {
            c.delta.new_files.len() + c.delta.deleted_ids.len() + c.delta.modified.len() > 10_000
        });
        if should_merge {
            let cache = self.search_cache.as_mut().unwrap();
            log::info!("Merging delta: {} new, {} deleted, {} modified",
                cache.delta.new_files.len(), cache.delta.deleted_ids.len(), cache.delta.modified.len());
            cache.merge_delta_to_base(&mut self.volumes, &self.vol_names);
        }
    }

    pub fn delta_count(&self) -> usize {
        self.search_cache.as_ref().map_or(0, |c| c.delta.new_files.len())
    }

    pub fn apply_incremental(&mut self, drive_letter: &str, result: &IncrementalResult) -> usize {
        // volume_files 现在是 &[FileEntry]
        let volume_files: Option<&[FileEntry]> = self.volumes.get(drive_letter).map(|v| v.files());
        let vol_idx = match self.volume_index.get(drive_letter).copied() {
            Some(i) => i,
            None => return 0,
        };

        let cache = match self.search_cache.as_mut() {
            Some(c) => c,
            None => return 0,
        };

        apply_incremental_to_cache(cache, &self.volumes, &self.vol_names, volume_files, vol_idx, result)
    }
}

/// 增量更新缓存（walkdir 路径）
fn apply_incremental_to_cache(
    cache: &mut SearchCache,
    volumes: &HashMap<String, VolumeMonitor>,
    vol_names: &[String],
    volume_files: Option<&[FileEntry]>,
    vol_idx: u8,
    result: &IncrementalResult,
) -> usize {
    let is_identity = result.new_file_indices.is_empty()
        && result.index_map.iter().enumerate().all(|(i, m)| *m == Some(i));
    if is_identity {
        return cache.base.matched.len();
    }

    let mut new_matched: Vec<(u8, u32)> = Vec::with_capacity(cache.base.matched.len());

    for (vol, idx) in cache.base.matched.drain(..) {
        if vol != vol_idx {
            new_matched.push((vol, idx));
        } else if (idx as usize) < result.index_map.len() {
            if let Some(new_idx) = result.index_map[idx as usize] {
                new_matched.push((vol, new_idx as u32));
            }
        }
    }

    let added_count_before = new_matched.len();
    if !result.new_file_indices.is_empty() {
        if let Some(files) = volume_files {
            let query = if cache.query.trim().is_empty() { None } else { Some(crate::search::query::SearchQuery::parse(&cache.query)) };
            let needs_path = query.as_ref().map_or(false, |q| q.path_filter.is_some());
            let path_table = volumes.get(&vol_names[vol_idx as usize]).map(|m| &m.path_table);

            for &new_idx in &result.new_file_indices {
                if new_idx >= files.len() { continue; }
                let file = &files[new_idx];
                if let Some(ref q) = query {
                    let full_path = if needs_path {
                        path_table.map(|pt| pt.resolve_file_path(file.path_id, &file.name)).unwrap_or_default()
                    } else {
                        CompactString::new("")
                    };
                    if !crate::search::query::SearchQuery::matches_entry(q, file, &full_path) { continue; }
                }
                if cache.files_only && file.is_directory { continue; }
                if cache.directories_only && !file.is_directory { continue; }
                new_matched.push((vol_idx, new_idx as u32));
            }
        }
    }
    let _added_count = new_matched.len() - added_count_before;

    cache.base.matched = new_matched;
    cache.base.sorted_by_name = None;
    cache.base.sorted_by_path = None;
    cache.base.sorted_by_size = None;
    cache.base.sorted_by_modified = None;

    cache.base.matched.len()
}

impl VolumeMonitor {
    pub fn new(drive_letter: String, include_hidden_files: bool, include_system_files: bool) -> Self {
        Self {
            drive_letter,
            files: Vec::new(),
            // 初始化路径前缀压缩表
            // PathTable 内部会预分配 50万容量并占用一个占位 entry
            path_table: PathTable::new(),
            include_hidden_files,
            include_system_files,
            fid_index: None,
            use_usn: false,
        }
    }

    /// 清理已删除条目（path_id == PathTable::deleted_id()）并重建 fid_index
    ///
    /// FileEntry 不再存储完整路径字符串，删除标记改为 path_id = u32::MAX
    /// 优化：仅当有文件被删除时才重建 fid_index（O(N) + O(N log N)）
    pub fn compact_files(&mut self) {
        let old_len = self.files.len();
        self.files.retain(|f| !PathTable::is_deleted(f.path_id));
        let removed = old_len - self.files.len();
        // 仅当有文件被删除时才重建 fid_index，避免每次轮询都做 O(N) + O(N log N) 操作
        if removed > 0 {
            let mut new_fid_index: Vec<(u32, u32)> = Vec::with_capacity(self.files.len());
            for (i, f) in self.files.iter().enumerate() {
                new_fid_index.push((f.file_id, i as u32));
            }
            new_fid_index.sort_unstable_by_key(|(id, _)| *id);
            self.fid_index = Some(new_fid_index);
        }
    }

    /// Update settings without recreating the monitor or clearing its file list.
    pub fn update_settings(&mut self, include_hidden_files: bool, include_system_files: bool) {
        self.include_hidden_files = include_hidden_files;
        self.include_system_files = include_system_files;
    }

    /// 返回内部 FileEntry 切片
    ///
    /// 注意：返回的是 FileEntry 而非 SearchResult
    /// 调用方需要通过 path_table.resolve_file_path(f.path_id, &f.name) 解析完整路径
    pub fn files(&self) -> &[FileEntry] {
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
        let start_id = self.files.len() as u32;
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

            // 文件：intern 父目录路径得到 path_id（FileEntry.path_id 指向父目录）
            // 目录：额外注册自身路径供子条目使用
            let parent_path = parent_dir_of(&path_str);
            let path_id = self.path_table.intern(parent_path);
            if is_dir {
                self.path_table.intern(&path_str);
            }
            let entry = FileEntry::new(
                name.into(),
                path_id,
                size,
                modified_ts,
                start_id + count as u32,
                is_dir,
            );

            self.files.push(entry);
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
        if self.use_usn {
            return Ok(IncrementalResult {
                added: 0,
                updated: 0,
                removed: 0,
                total: self.files.len(),
                index_map: Vec::new(),
                new_file_indices: Vec::new(),
            });
        }

        let include_hidden = self.include_hidden_files;
        let include_system = self.include_system_files;

        // 构建 (path_id, name)→index 映射
        // 由于 path_id 现在指向父目录，同一目录下的文件共享 path_id，
        // 必须用 (path_id, name) 组合作为唯一标识
        let mut path_map: HashMap<(u32, CompactString), usize> = HashMap::with_capacity(self.files.len());
        for (i, f) in self.files.iter().enumerate() {
            path_map.insert((f.path_id, f.name.clone()), i);
        }

        let mut visited: Vec<bool> = vec![false; self.files.len()];
        // 跟踪新增文件的索引（避免重复构造路径字符串）
        let mut added_indices: HashSet<usize> = HashSet::new();
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

            // 文件：intern 父目录路径得到 path_id（FileEntry.path_id 指向父目录）
            // 目录：额外注册自身路径供子条目使用
            let parent_path = parent_dir_of(&path_str);
            let path_id = self.path_table.intern(parent_path);
            if is_dir {
                self.path_table.intern(&path_str);
            }

            // 用 (path_id, name) 作为唯一标识查找
            let key = (path_id, CompactString::from(name.as_str()));
            if let Some(&idx) = path_map.get(&key) {
                // 已存在文件 — 检查是否被修改
                visited[idx] = true;
                let existing = &self.files[idx];
                // 注意：modified_time 在 FileEntry 中是 i32，需将 i64 的 modified_ts 转换为 i32 比较
                if existing.modified_time != modified_ts as i32 || existing.size != size {
                    self.files[idx] = FileEntry::new(
                        name.into(),
                        path_id,
                        size,
                        modified_ts,
                        existing.file_id,
                        is_dir,
                    );
                    updated += 1;
                }
            } else {
                // 新文件
                let file_id = self.files.len() as u32;
                self.files.push(FileEntry::new(
                    name.into(),
                    path_id,
                    size,
                    modified_ts,
                    file_id,
                    is_dir,
                ));
                added_indices.insert(self.files.len() - 1);
                path_map.insert(key, self.files.len() - 1);
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

        // 移除未访问的文件（已删除的文件）
        let old_len = self.files.len();
        let mut index_map = vec![None; old_len];
        // 预分配保守容量 — 通常 95%+ 的文件会存活
        let mut new_files = Vec::with_capacity(old_len * 95 / 100);
        // 用移动替代克隆，避免为字符串分配新内存
        for (i, file) in self.files.drain(..).enumerate() {
            if i < visited.len() && visited[i] {
                index_map[i] = Some(new_files.len());
                new_files.push(file);
            }
        }
        let removed = old_len - new_files.len();
        self.files = new_files;

        // 重新分配 file_id 以消除间隙
        for (i, f) in self.files.iter_mut().enumerate() {
            f.file_id = i as u32;
        }

        // 查找新增文件的索引用于缓存更新
        // 压缩后 added_indices 需要重映射，因为索引发生了偏移
        // 使用 visited 数组 + 原索引找到新位置
        let mut remapped_added: Vec<usize> = Vec::with_capacity(added_indices.len());
        for old_idx in added_indices.iter() {
            if *old_idx < index_map.len() {
                if let Some(new_idx) = index_map[*old_idx] {
                    remapped_added.push(new_idx);
                }
            }
        }
        remapped_added.sort_unstable();
        let new_file_indices = remapped_added;

        log::info!(
            "Incremental scan {}: +{} ~{} -{} (total: {})",
            drive_letter, added, updated, removed, self.files.len()
        );

        Ok(IncrementalResult { added, updated, removed, total: self.files.len(), index_map, new_file_indices })
    }

    /// 按完整路径移除文件（优化版：先比较文件名，仅名称匹配时才解析完整路径）
    ///
    /// 性能对比：
    /// - 旧版：221万次 resolve_file_path（O(depth) 路径拼接 + 字符串比较）≈ 500ms-2s
    /// - 新版：221万次 name == file_name（O(1) CompactString 比较）+ 极少量路径解析 ≈ <5ms
    pub fn remove_file(&mut self, file_path: &str) {
        // 提取目标文件名用于快速预过滤
        let file_name = std::path::Path::new(file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        let path_table = &self.path_table;
        self.files.retain(|f| {
            // 快速路径：文件名不匹配则直接保留（O(1)，跳过路径解析）
            if f.name.as_str() != file_name {
                return true;
            }
            // 慢路径：名称匹配时才解析完整路径确认（极罕见）
            let full_path = path_table.resolve_file_path(f.path_id, &f.name);
            full_path.as_str() != file_path
        });
    }
}