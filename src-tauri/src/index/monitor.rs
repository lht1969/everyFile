use crate::error::Result;
use crate::index::path_table::PathTable;
use crate::search::{FileEntry, SearchOptions, SearchResult, SortBy, SortDirection};
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
fn should_skip_by_attr(
    include_hidden: bool,
    include_system: bool,
    meta: &std::fs::Metadata,
) -> bool {
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

/// Base cache: stable data from full scan.
/// `valid_indices` is the single source of truth for sort order — an unsorted list of
/// indices into `matched` that reference valid file entries. Sort permutations are NO
/// longer cached; instead, `get_sorted_slice` uses `select_nth_unstable_by` (O(n))
/// + parallel partial sort of only the K needed entries (O(k log k)) on each call.
pub struct BaseCache {
    pub matched: Vec<(u8, u32)>,
    /// 有效索引缓存，值为 matched 的下标
    /// 仅当 valid_indices_matched_len == matched.len() 时才有效
    pub valid_indices: Option<Vec<u32>>,
    /// 构建 valid_indices 时 matched 的长度，用于验证缓存有效性
    /// 防止 matched 长度变化后索引越界
    pub valid_indices_matched_len: usize,
}

/// Delta cache: incremental changes. new_files is APPEND-ONLY (no swap_remove!).
pub struct DeltaCache {
    pub new_files: Vec<(u8, FileEntry)>,
    pub file_id_index: HashMap<u32, usize>,
    pub deleted_ids: HashSet<u32>,
    /// fids that were both deleted and re-added in the same batch (renames).
    /// These stay in `deleted_ids` to hide old base entries, but the new
    /// delta entry must NOT be filtered out by `deleted_ids`.
    pub renamed_fids: HashSet<u32>,
    pub modified: HashMap<u32, (u8, FileEntry)>,
    pub matched: Vec<(u8, u32)>,
    pub generation: u64,
}

/// 排序结果缓存，存储 base 部分的排序索引
///
/// `sorted_base` 保存的是对 `valid_indices` 完全排序后的结果，未预先过滤
/// deleted/modified 条目。跨 delta 变化复用时，在归并循环中按需跳过这些
/// 条目即可，避免每次请求都对整个 base 做全量过滤。
///
/// 注意：sorted_base 中的值是 base.matched 的索引，因此必须确保
/// 缓存创建时的 base.matched 长度与当前一致，否则索引可能越界。
///
/// sorted_base 仅依赖 base.matched（基础数据），不依赖 delta。
/// delta 变化由 merge 循环的 is_base_active lazy skip 处理，
/// 因此 sorted_base 可以跨 delta 变化复用，无需因 delta 变化而重建。
struct SortedCache {
    sort_by: SortBy,
    sort_direction: SortDirection,
    /// 使用 Arc 共享排序后的 base 索引，避免每次请求都 clone 整个 Vec。
    /// 对于 221 万文件，Vec<u32> 约 8.8MB，clone 会频繁触发内存分配与波动。
    sorted_base: std::sync::Arc<Vec<u32>>,
    /// 缓存创建时 base.matched 的长度，用于验证缓存有效性
    /// 防止 base.matched 长度变化后缓存索引越界
    base_matched_len: usize,
    /// 缓存创建时 valid_indices 对应的 matched 长度，用于验证缓存有效性
    /// 防止 valid_indices 重建后 sorted_base 基于旧的索引集合
    valid_indices_matched_len: usize,
}

pub struct SearchCache {
    cache_key: u64,
    query: String,
    files_only: bool,
    directories_only: bool,
    pub created_at: Instant,
    pub base: BaseCache,
    pub delta: DeltaCache,
    sorted_cache: Option<SortedCache>,
    /// 增量维护的 base 有效条目数（未被 delta.deleted_ids/modified 隐藏的条目数）。
    /// 避免每次 get_sorted_slice 都对 221 万 valid_indices 做 O(n) 全量扫描。
    /// None 表示需要从头计算（首次调用、merge 后、base.matched 变化后）。
    active_base_count: Option<usize>,
    /// base 中所有文件的 file_id 排序列表，用于 O(log n) 二分查找。
    /// 延迟构建：首次需要维护 active_base_count 时构建。
    base_file_ids: Option<Vec<u32>>,
}

impl SearchCache {
    pub fn is_valid(&self) -> bool {
        self.created_at.elapsed() < Duration::from_secs(CACHE_TTL_SECS)
    }

    pub fn refresh(&mut self) {
        self.created_at = Instant::now();
    }

    /// 确保 base_file_ids 已构建。返回 true 表示可用。
    fn ensure_base_file_ids(
        &mut self,
        volumes: &HashMap<String, VolumeMonitor>,
        vol_names: &[String],
    ) -> bool {
        if self.base_file_ids.is_some() {
            return true;
        }
        if self.base.valid_indices_matched_len != self.base.matched.len() {
            return false;
        }
        let mut fids: Vec<u32> = Vec::with_capacity(self.base.matched.len());
        for &(vol, file_idx) in &self.base.matched {
            let vi = vol as usize;
            if let Some(vol_name) = vol_names.get(vi) {
                if let Some(m) = volumes.get(vol_name) {
                    if (file_idx as usize) < m.files.len() {
                        fids.push(m.files[file_idx as usize].file_id);
                    }
                }
            }
        }
        fids.sort_unstable();
        fids.dedup();
        self.base_file_ids = Some(fids);
        true
    }

    /// O(log n) 检查 file_id 是否在 base 中
    fn is_fid_in_base(&self, fid: u32) -> bool {
        self.base_file_ids
            .as_ref()
            .map_or(false, |ids| ids.binary_search(&fid).is_ok())
    }

    /// 计算并缓存 active_base_count（首次调用时 O(n)，后续增量维护）
    fn ensure_active_base_count(
        &mut self,
        volumes: &HashMap<String, VolumeMonitor>,
        vol_names: &[String],
    ) {
        if self.active_base_count.is_some() {
            return;
        }
        if self.base.valid_indices_matched_len != self.base.matched.len() {
            // valid_indices 失效，需要从 matched 直接计算
            let count = self
                .base
                .matched
                .iter()
                .filter(|&&(vol, file_idx)| {
                    let Some(vol_name) = vol_names.get(vol as usize) else {
                        return false;
                    };
                    if let Some(m) = volumes.get(vol_name) {
                        if let Some(f) = m.files.get(file_idx as usize) {
                            return !self.delta.deleted_ids.contains(&f.file_id)
                                && !self.delta.modified.contains_key(&f.file_id);
                        }
                    }
                    false
                })
                .count();
            self.active_base_count = Some(count);
            return;
        }
        if let Some(ref valid_indices) = self.base.valid_indices {
            let count = valid_indices
                .iter()
                .filter(|&&idx| {
                    let Some(&(vol, file_idx)) = self.base.matched.get(idx as usize) else {
                        return false;
                    };
                    let Some(vol_name) = vol_names.get(vol as usize) else {
                        return false;
                    };
                    if let Some(m) = volumes.get(vol_name) {
                        if let Some(f) = m.files.get(file_idx as usize) {
                            return !self.delta.deleted_ids.contains(&f.file_id)
                                && !self.delta.modified.contains_key(&f.file_id);
                        }
                    }
                    false
                })
                .count();
            self.active_base_count = Some(count);
        } else {
            self.active_base_count = Some(0);
        }
    }

    /// 增量更新 active_base_count：当 fid 被添加到 delta.deleted_ids 或 delta.modified 时调用。
    /// 如果该 fid 在 base 中且之前未被隐藏，则 active_base_count -= 1。
    fn on_base_entry_hidden(&mut self, fid: u32) {
        if self.active_base_count.is_none() {
            return;
        }
        let in_base = self.is_fid_in_base(fid);
        let already_hidden =
            self.delta.deleted_ids.contains(&fid) || self.delta.modified.contains_key(&fid);
        if in_base && !already_hidden {
            if let Some(ref mut count) = self.active_base_count {
                *count = count.saturating_sub(1);
            }
        }
    }

    /// 增量更新 active_base_count：当 fid 从 delta.deleted_ids 或 delta.modified 中移除时调用。
    /// 如果该 fid 在 base 中且不再被任何 delta 隐藏，则 active_base_count += 1。
    fn on_base_entry_unhidden(&mut self, fid: u32) {
        if self.active_base_count.is_none() {
            return;
        }
        let in_base = self.is_fid_in_base(fid);
        let still_hidden =
            self.delta.deleted_ids.contains(&fid) || self.delta.modified.contains_key(&fid);
        if in_base && !still_hidden {
            if let Some(ref mut count) = self.active_base_count {
                *count += 1;
            }
        }
    }

    /// 使 active_base_count 和 base_file_ids 失效（base.matched 变化后调用）
    fn invalidate_base_count(&mut self) {
        self.active_base_count = None;
        self.base_file_ids = None;
    }

    /// Merge delta into base. Called periodically.
    pub fn merge_delta_to_base(
        &mut self,
        volumes: &mut HashMap<String, VolumeMonitor>,
        vol_names: &[String],
    ) {
        if self.delta.new_files.is_empty()
            && self.delta.deleted_ids.is_empty()
            && self.delta.modified.is_empty()
        {
            return;
        }
        let t0 = Instant::now();

        // Apply modifications to base.files
        for (&fid, &(vol_idx, ref new_entry)) in &self.delta.modified {
            // 安全访问 vol_names，越界时跳过
            let Some(vol_name) = vol_names.get(vol_idx as usize) else {
                continue;
            };
            if let Some(monitor) = volumes.get_mut(vol_name) {
                if let Some(fi) = monitor.fid_index.as_ref() {
                    if let Ok(pos) = fi.binary_search_by_key(&fid, |(id, _)| *id) {
                        // 安全访问 fi，越界时跳过
                        let Some(&(_, idx)) = fi.get(pos) else {
                            continue;
                        };
                        let idx = idx as usize;
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
                        // 安全访问 fi，越界时跳过
                        let Some(&(_, idx)) = fi.get(pos) else {
                            continue;
                        };
                        let idx = idx as usize;
                        if idx < monitor.files.len() {
                            monitor.files[idx].path_id = PathTable::deleted_id();
                        }
                    }
                }
            }
        }

        // Append delta.new_files to base.files
        for (vol_idx, file_entry) in &self.delta.new_files {
            // 安全访问 vol_names，越界时跳过
            let Some(vol_name) = vol_names.get(*vol_idx as usize) else {
                continue;
            };
            if let Some(monitor) = volumes.get_mut(vol_name) {
                monitor.files.push(file_entry.clone());
            }
        }

        // Rebuild fid_index for affected volumes and compact
        let mut affected_vols: HashSet<u8> = self.delta.new_files.iter().map(|(v, _)| *v).collect();
        for &fid in self
            .delta
            .modified
            .keys()
            .chain(self.delta.deleted_ids.iter())
        {
            for (idx, (_vn, monitor)) in volumes.iter().enumerate() {
                if let Some(fi) = monitor.fid_index.as_ref() {
                    if fi.binary_search_by_key(&fid, |(id, _)| *id).is_ok() {
                        affected_vols.insert(idx as u8);
                    }
                }
            }
        }
        for vol_idx in &affected_vols {
            // 安全访问 vol_names，越界时跳过
            let Some(vol_name) = vol_names.get(*vol_idx as usize) else {
                continue;
            };
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
        self.delta.renamed_fids.clear();
        self.delta.modified.clear();
        self.delta.matched.clear();
        self.delta.generation += 1;
        // merge 后 delta 为空，generation 已递增；排序缓存会因 base_matched_len
        // 变化而失效，此处无需额外处理。
        self.sorted_cache = None;

        // Rebuild base matched from scratch
        let mut matched = Vec::new();
        for (vol_name, monitor) in volumes.iter() {
            let vol_idx = vol_names.iter().position(|n| n == vol_name).unwrap() as u8;
            for (file_idx, file) in monitor.files.iter().enumerate() {
                if PathTable::is_deleted(file.path_id) {
                    continue;
                }
                if self.files_only && file.is_directory {
                    continue;
                }
                if self.directories_only && !file.is_directory {
                    continue;
                }
                matched.push((vol_idx, file_idx as u32));
            }
        }
        self.base.matched = matched;

        // Invalidate base valid_indices (will be rebuilt lazily)
        self.base.valid_indices = None;
        self.base.valid_indices_matched_len = 0;
        // 失效增量维护的 base count（base.matched 已重建）
        self.invalidate_base_count();

        log::info!(
            "merge_delta_to_base: done in {:?}, base={} entries",
            t0.elapsed(),
            self.base.matched.len()
        );
    }

    pub fn get_sorted_slice(
        &mut self,
        volumes: &HashMap<String, VolumeMonitor>,
        vol_names: &[String],
        sort_by: SortBy,
        sort_direction: SortDirection,
        start: usize,
        end: usize,
    ) -> (Vec<SearchResult>, usize) {
        let t0 = Instant::now();
        log::debug!(
            "[SORT] slice start={} end={} sort={:?}/{:?} matched={} delta={}/{}/{}",
            start,
            end,
            sort_by,
            sort_direction,
            self.base.matched.len(),
            self.delta.new_files.len(),
            self.delta.deleted_ids.len(),
            self.delta.modified.len()
        );

        // Snapshot delta to avoid inconsistency if it's modified during this call.
        // We iterate over self.delta fields directly instead of cloning,
        // saving ~50MB+ allocation per call for large deltas.
        // The is_base_active closure and merge loop also reference self.delta directly.
        // This is safe because self is not mutably borrowed when they run.

        // Build valid_indices if needed (unsorted list of valid indices into matched)
        // 验证 valid_indices 的有效性：必须与当前 matched.len() 一致
        // 否则说明 matched 长度已变化，缓存的索引可能越界
        if self.base.valid_indices.is_some()
            && self.base.valid_indices_matched_len != self.base.matched.len()
        {
            self.base.valid_indices = None;
        }
        if self.base.valid_indices.is_none() && !self.base.matched.is_empty() {
            self.base.valid_indices =
                Some(build_valid_indices(&self.base.matched, volumes, vol_names));
            self.base.valid_indices_matched_len = self.base.matched.len();
        }

        // 提前计算 base_n（需要 &mut self），必须在 valid_indices 借用之前完成
        let base_n = if self.delta.deleted_ids.is_empty() && self.delta.modified.is_empty() {
            if self.base.valid_indices_matched_len == self.base.matched.len() {
                self.base.valid_indices.as_ref().map_or(0, |v| v.len())
            } else {
                self.base.matched.len()
            }
        } else {
            self.ensure_active_base_count(volumes, vol_names);
            self.active_base_count.unwrap_or(0)
        };

        // `sorted_base` 将基于未过滤的 valid_indices 构建，deleted/modified 的过滤
        // 推迟到归并阶段按需跳过，避免每次请求都扫描整个 base。
        let valid_indices = match self.base.valid_indices {
            Some(ref v) => v.as_slice(),
            None => return (Vec::new(), 0),
        };

        // Build sorted delta entries directly from self.delta.new_files
        // No index indirection - sort (index, key) tuples directly
        let query = if self.query.trim().is_empty() {
            None
        } else {
            Some(crate::search::query::SearchQuery::parse(&self.query))
        };
        let needs_path = query.as_ref().is_some_and(|q| q.path_filter.is_some());
        // delta_entries: (&FileEntry, vol_idx, path_ordinal)
        // 直接持有 FileEntry 引用，避免中间索引转换
        let mut delta_entries: Vec<(&FileEntry, u8, u32)> = Vec::new();
        for (vol, f) in &self.delta.new_files {
            if self.delta.deleted_ids.contains(&f.file_id) && !self.delta.renamed_fids.contains(&f.file_id) {
                continue;
            }
            // 安全访问 vol_names，越界时跳过
            let Some(vol_name) = vol_names.get(*vol as usize) else {
                continue;
            };
            if let Some(ref q) = query {
                if let Some(m) = volumes.get(vol_name) {
                    let full_path = if needs_path {
                        m.path_table.resolve_file_path(f.path_id, &f.name)
                    } else {
                        CompactString::new("")
                    };
                    if !crate::search::query::SearchQuery::matches_entry(q, f, &full_path) {
                        continue;
                    }
                }
            }
            if self.files_only && f.is_directory {
                continue;
            }
            if self.directories_only && !f.is_directory {
                continue;
            }
            let ordinal = volumes
                .get(vol_name)
                .map(|m| m.path_table.get_ordinal(f.path_id))
                .unwrap_or(u32::MAX);
            delta_entries.push((f, *vol, ordinal));
        }
        // 加入 delta.modified 的新值：modified 的旧值在 cache_hit 复用时从 base 过滤，新值在此参与排序归并
        // 注意：必须过滤 self.delta.deleted_ids，避免"先 modified 后 deleted"场景下已删除文件出现
        //       （apply_incremental_usn 的 Phase 2 不会从 delta.modified 移除 deleted 的 fid）
        for (vol, f) in self.delta.modified.values() {
            if self.delta.deleted_ids.contains(&f.file_id) {
                continue;
            }
            // 安全访问 vol_names，越界时跳过
            let Some(vol_name) = vol_names.get(*vol as usize) else {
                continue;
            };
            if let Some(ref q) = query {
                if let Some(m) = volumes.get(vol_name) {
                    let full_path = if needs_path {
                        m.path_table.resolve_file_path(f.path_id, &f.name)
                    } else {
                        CompactString::new("")
                    };
                    if !crate::search::query::SearchQuery::matches_entry(q, f, &full_path) {
                        continue;
                    }
                }
            }
            if self.files_only && f.is_directory {
                continue;
            }
            if self.directories_only && !f.is_directory {
                continue;
            }
            let ordinal = volumes
                .get(vol_name)
                .map(|m| m.path_table.get_ordinal(f.path_id))
                .unwrap_or(u32::MAX);
            delta_entries.push((f, *vol, ordinal));
        }

        // Delta entries 始终按升序排列；base 同样按升序排序，升序归并从头取，
        // 降序归并从尾部取，这样可以用同一套有序数组服务两种方向。
        // 使用 file_id 作为 tie-breaker，保证同键值 delta 条目内部顺序稳定，
        // 避免相同大小/名称/时间的文件在多次请求中顺序跳变。
        match sort_by {
            SortBy::Name | SortBy::Score => {
                delta_entries.sort_by(|a, b| {
                    a.0.name
                        .as_str()
                        .cmp(b.0.name.as_str())
                        .then(a.0.file_id.cmp(&b.0.file_id))
                });
            }
            SortBy::Path => {
                // delta 与 base 使用相同的 (vol_name, path_ordinal, name, file_id) 键
                delta_entries.sort_by(|a, b| {
                    let va = vol_names
                        .get(a.1 as usize)
                        .map(|s| s.as_str())
                        .unwrap_or("");
                    let vb = vol_names
                        .get(b.1 as usize)
                        .map(|s| s.as_str())
                        .unwrap_or("");
                    va.cmp(vb)
                        .then(a.2.cmp(&b.2))
                        .then(a.0.name.as_str().cmp(b.0.name.as_str()))
                        .then(a.0.file_id.cmp(&b.0.file_id))
                });
            }
            SortBy::Size => {
                delta_entries
                    .sort_by(|a, b| a.0.size.cmp(&b.0.size).then(a.0.file_id.cmp(&b.0.file_id)));
            }
            SortBy::ModifiedTime => {
                delta_entries.sort_by(|a, b| {
                    a.0.modified_time
                        .cmp(&b.0.modified_time)
                        .then(a.0.file_id.cmp(&b.0.file_id))
                });
            }
        }

        let delta_n = delta_entries.len();

        // 准备按卷访问的切片，slow path 排序时使用
        let vol_files: Vec<&[FileEntry]> = (0..vol_names.len())
            .map(|i| {
                volumes
                    .get(&vol_names[i])
                    .map(|v| v.files.as_slice())
                    .unwrap_or(&[])
            })
            .collect();
        let vol_path_tables: Vec<Option<&PathTable>> = (0..vol_names.len())
            .map(|i| volumes.get(&vol_names[i]).map(|v| &v.path_table))
            .collect();

        // cache_hit 检查：排序字段/方向、base.matched 长度、valid_indices 长度
        // 均未变化时复用 sorted_base。
        // sorted_base 保存的是未过滤的 valid_indices 全排序结果，delta 变化时
        // 在归并阶段 lazy skip 即可（is_base_active），无需因 delta 变化而重建。
        let cache_hit = self.sorted_cache.as_ref().is_some_and(|sc| {
            sc.sort_by == sort_by
                && sc.sort_direction == sort_direction
                && sc.base_matched_len == self.base.matched.len()
                && sc.valid_indices_matched_len == self.base.valid_indices_matched_len
        });
        log::info!(
            "[SORT] cache_hit={} valid_indices_len={} delta_gen={}",
            cache_hit,
            valid_indices.len(),
            self.delta.generation
        );

        let sorted_base: std::sync::Arc<Vec<u32>> = if cache_hit {
            // Arc 克隆只增加引用计数，不分配新内存，避免大 Vec 复制。
            self.sorted_cache.as_ref().unwrap().sorted_base.clone()
        } else {
            // Slow path: 对 valid_indices 做全量排序并缓存。
            // 之前使用 select_nth_unstable_by 做部分排序，导致 sorted_base 中超出
            // 窗口的部分无序，后续 cache_hit 请求访问到这些位置时显示乱序。
            let sorted: Vec<u32> = match sort_by {
                SortBy::Name | SortBy::Score => {
                    // 使用 (name, file_id) 作为排序键，file_id 作为 tie-breaker
                    // 保证与 delta_entries 的排序顺序完全一致，避免归并时相同名称
                    // 的条目顺序跳变。
                    let mut entries: Vec<(&str, u32, u32)> = valid_indices
                        .par_iter()
                        .map(|&idx| {
                            let (vol, file_idx) = self
                                .base
                                .matched
                                .get(idx as usize)
                                .copied()
                                .unwrap_or((0, 0));
                            let vi = vol as usize;
                            let (name, fid) = if vi < vol_files.len()
                                && !vol_files[vi].is_empty()
                                && (file_idx as usize) < vol_files[vi].len()
                            {
                                let f = &vol_files[vi][file_idx as usize];
                                (f.name.as_str(), f.file_id)
                            } else {
                                ("", u32::MAX)
                            };
                            (name, fid, idx)
                        })
                        .collect();
                    entries.par_sort_unstable_by(|a, b| {
                        a.0.cmp(b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2))
                    });
                    entries.into_iter().map(|(_, _, idx)| idx).collect()
                }
                SortBy::Path => {
                    // 使用 (vol_name, path_ordinal, name, file_id) 作为排序键。
                    // 跨卷场景下必须先把盘符纳入比较，否则不同卷的 ordinal 来自各
                    // 自独立的 PathTable，直接比较没有意义。
                    let mut entries: Vec<(&str, u32, &str, u32, u32)> = valid_indices
                        .par_iter()
                        .map(|&idx| {
                            let (vol, file_idx) = self
                                .base
                                .matched
                                .get(idx as usize)
                                .copied()
                                .unwrap_or((0, 0));
                            let vi = vol as usize;
                            let vol_name = vol_names.get(vi).map(|s| s.as_str()).unwrap_or("");
                            if vi < vol_path_tables.len()
                                && vol_path_tables[vi].is_some()
                                && vi < vol_files.len()
                                && !vol_files[vi].is_empty()
                                && (file_idx as usize) < vol_files[vi].len()
                            {
                                let pt = vol_path_tables[vi].unwrap();
                                let f = &vol_files[vi][file_idx as usize];
                                (
                                    vol_name,
                                    pt.get_ordinal(f.path_id),
                                    f.name.as_str(),
                                    f.file_id,
                                    idx,
                                )
                            } else {
                                (vol_name, u32::MAX, "", u32::MAX, idx)
                            }
                        })
                        .collect();
                    entries.par_sort_unstable_by(|a, b| {
                        a.0.cmp(b.0)
                            .then(a.1.cmp(&b.1))
                            .then(a.2.cmp(b.2))
                            .then(a.3.cmp(&b.3))
                            .then(a.4.cmp(&b.4))
                    });
                    entries.into_iter().map(|(_, _, _, _, idx)| idx).collect()
                }
                SortBy::Size => {
                    // 使用 (size, file_id) 作为排序键，file_id 作为 tie-breaker
                    // 与 delta_entries 的 Size 排序保持一致，避免同大小文件乱序。
                    let mut entries: Vec<(u64, u32, u32)> = valid_indices
                        .par_iter()
                        .map(|&idx| {
                            let (vol, file_idx) = self
                                .base
                                .matched
                                .get(idx as usize)
                                .copied()
                                .unwrap_or((0, 0));
                            let vi = vol as usize;
                            let (size, fid) = if vi < vol_files.len()
                                && !vol_files[vi].is_empty()
                                && (file_idx as usize) < vol_files[vi].len()
                            {
                                let f = &vol_files[vi][file_idx as usize];
                                (f.size, f.file_id)
                            } else {
                                (u64::MAX, u32::MAX)
                            };
                            (size, fid, idx)
                        })
                        .collect();
                    entries.par_sort_unstable_by(|a, b| {
                        a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2))
                    });
                    entries.into_iter().map(|(_, _, idx)| idx).collect()
                }
                SortBy::ModifiedTime => {
                    // 使用 (modified_time, file_id) 作为排序键，file_id 作为 tie-breaker
                    // 与 delta_entries 的 ModifiedTime 排序保持一致。
                    let mut entries: Vec<(i32, u32, u32)> = valid_indices
                        .par_iter()
                        .map(|&idx| {
                            let (vol, file_idx) = self
                                .base
                                .matched
                                .get(idx as usize)
                                .copied()
                                .unwrap_or((0, 0));
                            let vi = vol as usize;
                            let (mt, fid) = if vi < vol_files.len()
                                && !vol_files[vi].is_empty()
                                && (file_idx as usize) < vol_files[vi].len()
                            {
                                let f = &vol_files[vi][file_idx as usize];
                                (f.modified_time, f.file_id)
                            } else {
                                (i32::MAX, u32::MAX)
                            };
                            (mt, fid, idx)
                        })
                        .collect();
                    entries.par_sort_unstable_by(|a, b| {
                        a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2))
                    });
                    entries.into_iter().map(|(_, _, idx)| idx).collect()
                }
            };

            self.sorted_cache = Some(SortedCache {
                sort_by,
                sort_direction,
                sorted_base: std::sync::Arc::new(sorted),
                base_matched_len: self.base.matched.len(),
                valid_indices_matched_len: self.base.valid_indices_matched_len,
            });
            // sorted 已移入 Arc，需从缓存重新取引用继续后续逻辑
            self.sorted_cache.as_ref().unwrap().sorted_base.clone()
        };

        // 判断 sorted_base 中某个 matched 索引当前是否仍然可显示（未被 delta
        // deleted/modified 过滤掉）。在 base_n 计算和归并阶段都会使用，实现
        // lazy skip。
        let is_base_active = |idx: u32| -> bool {
            let Some(&(vol, file_idx)) = self.base.matched.get(idx as usize) else {
                return false;
            };
            let Some(vol_name) = vol_names.get(vol as usize) else {
                return false;
            };
            if let Some(m) = volumes.get(vol_name) {
                if let Some(f) = m.files.get(file_idx as usize) {
                    return !self.delta.deleted_ids.contains(&f.file_id)
                        && !self.delta.modified.contains_key(&f.file_id);
                }
            }
            false
        };

        // 计算实际可显示总数 total_n = base_n + delta_n。
        // base_n 已在 delta_entries 构建前提前计算（增量维护），避免 O(n) 全量扫描。
        let total_n = base_n + delta_n;
        log::debug!(
            "[SORT] total_n={} delta_n={} base_n={}",
            total_n,
            delta_n,
            base_n
        );
        if total_n == 0 {
            return (Vec::new(), 0);
        }

        let (eff_start, eff_end) = match sort_direction {
            SortDirection::Ascending => (start.min(total_n), end.min(total_n)),
            SortDirection::Descending => {
                let s = total_n.saturating_sub(end.max(start)).min(total_n);
                let e = total_n.saturating_sub(start);
                (s, e.max(s))
            }
        };
        if eff_start >= eff_end || eff_start >= total_n {
            return (Vec::new(), total_n);
        }

        log::debug!(
            "[SORT] merge delta_n={} total_n={} eff_start={} eff_end={}",
            delta_n,
            total_n,
            eff_start,
            eff_end
        );

        match sort_direction {
            SortDirection::Ascending => {
                let mut results = Vec::with_capacity(eff_end - eff_start);
                let mut bi = 0usize;
                let mut di = 0usize;
                let mut pos = 0usize;
                while pos < eff_end && (bi < sorted_base.len() || di < delta_n) {
                    // 跳过已失效的 base 条目
                    while bi < sorted_base.len() && !is_base_active(sorted_base[bi]) {
                        bi += 1;
                    }

                    let pick_base = if di >= delta_n {
                        true
                    } else if bi >= sorted_base.len() {
                        false
                    } else {
                        let idx = sorted_base[bi];
                        let Some(&(va, fa_idx)) = self.base.matched.get(idx as usize) else {
                            bi += 1;
                            continue;
                        };
                        let Some(vol_a) = vol_names.get(va as usize) else {
                            bi += 1;
                            continue;
                        };
                        let Some(ma) = volumes.get(vol_a) else {
                            return (results, total_n);
                        };
                        let Some(fa) = ma.files.get(fa_idx as usize) else {
                            bi += 1;
                            continue;
                        };
                        let a_ordinal = ma.path_table.get_ordinal(fa.path_id);
                        let Some((fb, vb, b_ordinal)) = delta_entries.get(di) else {
                            bi += 1;
                            continue;
                        };
                        // 归并比较必须包含 file_id 作为 tie-breaker，否则相同主键的
                        // base/delta 条目顺序不稳定，导致显示乱序。
                        // Path 排序还需先把盘符纳入比较，避免跨卷 ordinal 不可比。
                        let ord = match sort_by {
                            SortBy::Name | SortBy::Score => fa
                                .name
                                .as_str()
                                .cmp(fb.name.as_str())
                                .then(fa.file_id.cmp(&fb.file_id)),
                            SortBy::Path => {
                                let vol_a_name =
                                    vol_names.get(va as usize).map(|s| s.as_str()).unwrap_or("");
                                let vol_b_name = vol_names
                                    .get(*vb as usize)
                                    .map(|s| s.as_str())
                                    .unwrap_or("");
                                vol_a_name
                                    .cmp(vol_b_name)
                                    .then(a_ordinal.cmp(b_ordinal))
                                    .then(fa.name.as_str().cmp(fb.name.as_str()))
                                    .then(fa.file_id.cmp(&fb.file_id))
                            }
                            SortBy::Size => fa.size.cmp(&fb.size).then(fa.file_id.cmp(&fb.file_id)),
                            SortBy::ModifiedTime => fa
                                .modified_time
                                .cmp(&fb.modified_time)
                                .then(fa.file_id.cmp(&fb.file_id)),
                        };
                        ord != std::cmp::Ordering::Greater
                    };

                    if pos >= eff_start {
                        if pick_base {
                            let idx = sorted_base[bi];
                            if let Some(&(vol, file_idx)) = self.base.matched.get(idx as usize) {
                                if let Some(vol_name) = vol_names.get(vol as usize) {
                                    if let Some(m) = volumes.get(vol_name) {
                                        if let Some(f) = m.files.get(file_idx as usize) {
                                            let full_path =
                                                m.path_table.resolve_file_path(f.path_id, &f.name);
                                            results.push(f.to_search_result(full_path));
                                        }
                                    }
                                }
                            }
                        } else if let Some((f, vol, _ordinal)) = delta_entries.get(di) {
                            if let Some(vol_name) = vol_names.get(*vol as usize) {
                                if let Some(m) = volumes.get(vol_name) {
                                    let full_path =
                                        m.path_table.resolve_file_path(f.path_id, &f.name);
                                    results.push(f.to_search_result(full_path));
                                }
                            }
                        }
                    }
                    if pick_base {
                        bi += 1;
                    } else {
                        di += 1;
                    }
                    pos += 1;
                }
                log::info!(
                    "[SORT] asc results={} total_n={} eff_start={} eff_end={} final_bi={} final_di={} final_pos={} elapsed={:?}",
                    results.len(),
                    total_n,
                    eff_start,
                    eff_end,
                    bi,
                    di,
                    pos,
                    t0.elapsed()
                );
                (results, total_n)
            }
            SortDirection::Descending => {
                let mut results = Vec::with_capacity(eff_end - eff_start);
                let mut bi = sorted_base.len();
                let mut di = delta_n;
                let mut pos = total_n;
                while pos > eff_start && (bi > 0 || di > 0) {
                    pos -= 1;

                    // 跳过已失效的 base 条目（从尾部向前跳过）
                    while bi > 0 && !is_base_active(sorted_base[bi - 1]) {
                        bi -= 1;
                    }

                    let pick_base = if di == 0 {
                        true
                    } else if bi == 0 {
                        false
                    } else {
                        let idx = sorted_base[bi - 1];
                        let Some(&(va, fa_idx)) = self.base.matched.get(idx as usize) else {
                            bi -= 1;
                            continue;
                        };
                        let Some(vol_a) = vol_names.get(va as usize) else {
                            bi -= 1;
                            continue;
                        };
                        let Some(ma) = volumes.get(vol_a) else {
                            return (results, total_n);
                        };
                        let Some(fa) = ma.files.get(fa_idx as usize) else {
                            bi -= 1;
                            continue;
                        };
                        let a_ordinal = ma.path_table.get_ordinal(fa.path_id);
                        let Some((fb, vb, b_ordinal)) = delta_entries.get(di - 1) else {
                            bi -= 1;
                            continue;
                        };
                        // 降序归并同样使用 file_id 作为 tie-breaker，保证全局有序。
                        // Path 排序还需先把盘符纳入比较，避免跨卷 ordinal 不可比。
                        let ord = match sort_by {
                            SortBy::Name | SortBy::Score => fa
                                .name
                                .as_str()
                                .cmp(fb.name.as_str())
                                .then(fa.file_id.cmp(&fb.file_id)),
                            SortBy::Path => {
                                let vol_a_name =
                                    vol_names.get(va as usize).map(|s| s.as_str()).unwrap_or("");
                                let vol_b_name = vol_names
                                    .get(*vb as usize)
                                    .map(|s| s.as_str())
                                    .unwrap_or("");
                                vol_a_name
                                    .cmp(vol_b_name)
                                    .then(a_ordinal.cmp(b_ordinal))
                                    .then(fa.name.as_str().cmp(fb.name.as_str()))
                                    .then(fa.file_id.cmp(&fb.file_id))
                            }
                            SortBy::Size => fa.size.cmp(&fb.size).then(fa.file_id.cmp(&fb.file_id)),
                            SortBy::ModifiedTime => fa
                                .modified_time
                                .cmp(&fb.modified_time)
                                .then(fa.file_id.cmp(&fb.file_id)),
                        };
                        ord != std::cmp::Ordering::Less
                    };

                    if pos < eff_end {
                        if pick_base {
                            bi -= 1;
                            let idx = sorted_base[bi];
                            if let Some(&(vol, file_idx)) = self.base.matched.get(idx as usize) {
                                if let Some(vol_name) = vol_names.get(vol as usize) {
                                    if let Some(m) = volumes.get(vol_name) {
                                        if let Some(f) = m.files.get(file_idx as usize) {
                                            let full_path =
                                                m.path_table.resolve_file_path(f.path_id, &f.name);
                                            results.push(f.to_search_result(full_path));
                                        }
                                    }
                                }
                            }
                        } else {
                            di -= 1;
                            if let Some((f, vol, _ordinal)) = delta_entries.get(di) {
                                if let Some(vol_name) = vol_names.get(*vol as usize) {
                                    if let Some(m) = volumes.get(vol_name) {
                                        let full_path =
                                            m.path_table.resolve_file_path(f.path_id, &f.name);
                                        results.push(f.to_search_result(full_path));
                                    }
                                }
                            }
                        }
                    } else {
                        if pick_base {
                            bi -= 1;
                        } else {
                            di -= 1;
                        }
                    }
                }
                log::info!(
                    "[SORT] desc results={} total_n={} eff_start={} eff_end={} final_bi={} final_di={} final_pos={} elapsed={:?}",
                    results.len(),
                    total_n,
                    eff_start,
                    eff_end,
                    bi,
                    di,
                    pos,
                    t0.elapsed()
                );
                (results, total_n)
            }
        }
    }
}

/// Build unsorted list of valid indices into `matched`.
/// Filters out entries whose volume or file index is out of bounds.
/// This is O(n) and called once per cache build; subsequent sorts use
/// `select_nth_unstable_by` on this list instead of a full O(n log n) sort.
fn build_valid_indices(
    matched: &[(u8, u32)],
    volumes: &HashMap<String, VolumeMonitor>,
    vol_names: &[String],
) -> Vec<u32> {
    let n = matched.len();
    if n == 0 {
        return Vec::new();
    }
    let vol_files: Vec<&[FileEntry]> = (0..vol_names.len())
        .map(|i| {
            volumes
                .get(&vol_names[i])
                .map(|v| v.files.as_slice())
                .unwrap_or(&[])
        })
        .collect();
    let mut v: Vec<u32> = Vec::with_capacity(n);
    for i in 0..n as u32 {
        let (vol, idx) = matched[i as usize];
        let vi = vol as usize;
        if vi < vol_files.len() && !vol_files[vi].is_empty() && (idx as usize) < vol_files[vi].len()
        {
            v.push(i);
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

impl Default for VolumeManager {
    fn default() -> Self {
        Self::new()
    }
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

    pub fn add_volume(
        &mut self,
        drive_letter: &str,
        _is_admin: bool,
        include_hidden_files: bool,
        include_system_files: bool,
    ) -> Result<()> {
        let idx = self.vol_names.len() as u8;
        self.volume_index.insert(drive_letter.to_string(), idx);
        self.vol_names.push(drive_letter.to_string());
        let monitor = VolumeMonitor::new(
            drive_letter.to_string(),
            include_hidden_files,
            include_system_files,
        );
        self.volumes.insert(drive_letter.to_string(), monitor);
        Ok(())
    }

    pub fn remove_volume(&mut self, drive_letter: &str) {
        self.volumes.remove(drive_letter);
        if let Some(idx) = self.volume_index.remove(drive_letter) {
            // 安全访问 vol_names，越界时跳过
            if let Some(name) = self.vol_names.get_mut(idx as usize) {
                name.clear();
            }
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

    /// 当前搜索实际可显示的总条目数，用于前端同步滚动条高度。
    /// 每次调用都重新计算，避免缓存值与实际可显示条目数不一致导致滚动条
    /// 高度错误或底部空白。
    pub fn search_with_options(
        &mut self,
        query: &str,
        options: &SearchOptions,
    ) -> (Vec<SearchResult>, usize) {
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
                let (first_batch, total) = old.get_sorted_slice(
                    &self.volumes,
                    &self.vol_names,
                    options.sort_by,
                    options.sort_direction,
                    0,
                    50,
                );
                log::info!("search_with_options total: {:?}", t0.elapsed());
                return (first_batch, total);
            }
        }

        // New search: build base matched from all volumes
        self.search_cache = None;
        let total_files: usize = self.volumes.values().map(|v| v.files.len()).sum();
        let matched_lock = std::sync::Mutex::new(Vec::with_capacity(total_files / 4));
        let is_empty_query = query.trim().is_empty();
        let parsed_query = if is_empty_query {
            None
        } else {
            Some(crate::search::query::SearchQuery::parse(query))
        };
        let query_controls_dir = parsed_query
            .as_ref()
            .is_some_and(|q| q.path_filter_dir_only);
        let needs_path = parsed_query
            .as_ref()
            .is_some_and(|q| q.path_filter.is_some());
        let files_only = options.files_only;
        let directories_only = options.directories_only;

        self.volumes.par_iter().for_each(|(vol_key, monitor)| {
            let vol_idx = self.volume_index[vol_key];
            if is_empty_query {
                let local: Vec<(u8, u32)> = monitor
                    .files
                    .par_iter()
                    .enumerate()
                    .filter_map(|(idx, file)| {
                        if files_only && file.is_directory {
                            return None;
                        }
                        if directories_only && !file.is_directory {
                            return None;
                        }
                        Some((vol_idx, idx as u32))
                    })
                    .collect();
                matched_lock.lock().unwrap().extend(local);
            } else {
                let pq = parsed_query.as_ref().unwrap();
                let local: Vec<(u8, u32)> = monitor
                    .files
                    .par_iter()
                    .enumerate()
                    .filter_map(|(idx, file)| {
                        let full_path = if needs_path {
                            monitor
                                .path_table
                                .resolve_file_path(file.path_id, &file.name)
                        } else {
                            CompactString::new("")
                        };
                        if !crate::search::query::SearchQuery::matches_entry(pq, file, &full_path) {
                            return None;
                        }
                        if !query_controls_dir && files_only && file.is_directory {
                            return None;
                        }
                        if directories_only && !file.is_directory {
                            return None;
                        }
                        Some((vol_idx, idx as u32))
                    })
                    .collect();
                matched_lock.lock().unwrap().extend(local);
            }
        });

        let all_matched = matched_lock.into_inner().unwrap();
        let total = all_matched.len();
        log::info!(
            "search_with_options: matched {} files, {:?}",
            total,
            t0.elapsed()
        );

        self.search_cache = Some(SearchCache {
            cache_key: new_cache_key,
            query: query.to_string(),
            files_only: options.files_only,
            directories_only: options.directories_only,
            created_at: Instant::now(),
            base: BaseCache {
                matched: all_matched,
                valid_indices: None,          // built lazily in get_sorted_slice
                valid_indices_matched_len: 0, // 与 valid_indices 同步，首次构建时设置
            },
            delta: DeltaCache {
                new_files: Vec::new(),
                file_id_index: HashMap::new(),
                deleted_ids: HashSet::new(),
                renamed_fids: HashSet::new(),
                modified: HashMap::new(),
                matched: Vec::new(),
                generation: 0,
            },
            sorted_cache: None,
            active_base_count: None, // lazily computed on first get_sorted_slice call
            base_file_ids: None,     // lazily built when needed for incremental tracking
        });

        // Use get_sorted_slice for first_batch (handles base+delta merge)
        let (first_batch, total) = self.search_cache.as_mut().unwrap().get_sorted_slice(
            &self.volumes,
            &self.vol_names,
            options.sort_by,
            options.sort_direction,
            0,
            50,
        );
        log::info!("search_with_options total: {:?}", t0.elapsed());
        (first_batch, total)
    }

    pub fn scan_all_with_progress(&mut self, handle: &tauri::AppHandle) -> Result<usize> {
        let mut total = 0;
        for (drive_letter, monitor) in self.volumes.iter_mut() {
            let count = monitor.scan_with_progress_callback(handle)?;
            log::info!("Scanned volume {}: {} files", drive_letter, count);
            total += count;
            let _ = handle.emit(
                "scan-complete",
                serde_json::json!({"volume": drive_letter, "count": count}),
            );
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

    pub fn get_cached_slice(
        &mut self,
        sort_by: SortBy,
        sort_direction: SortDirection,
        start: usize,
        end: usize,
    ) -> Option<(Vec<SearchResult>, usize)> {
        let cache = self.search_cache.as_mut()?;
        if !cache.is_valid() {
            return None;
        }
        cache.refresh();
        let (results, total) = cache.get_sorted_slice(
            &self.volumes,
            &self.vol_names,
            sort_by,
            sort_direction,
            start,
            end,
        );
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
    ///   Apply incremental USN changes. base.files is NOT modified (no compaction).
    ///   - Updates: modify base.files in-place (index stays valid)
    ///   - Deletions: record in delta.deleted_ids
    ///   - Additions: append to delta.new_files (append-only, no swap_remove!)
    pub fn apply_incremental_usn(
        &mut self,
        drive_letter: &str,
        added: Vec<SearchResult>,
        removed: Vec<(u64, String)>,
        updated: Vec<(u64, SearchResult)>,
    ) {
        // 无实质变更时直接返回，避免重建 delta.matched、递增 generation 和触发前端刷新
        if added.is_empty() && removed.is_empty() && updated.is_empty() {
            return;
        }

        let vol_idx = match self.volume_index.get(drive_letter).copied() {
            Some(i) => i,
            None => return,
        };
        let has_cache = self.search_cache.is_some();

        // 确保 base_file_ids 已构建，供后续增量维护 active_base_count 使用
        if has_cache {
            if let Some(cache) = self.search_cache.as_mut() {
                cache.ensure_base_file_ids(&self.volumes, &self.vol_names);
            }
        }

        // Phase 1: Updates - store in delta.modified, NOT in base.files
        // 同时处理"delta.new_files 中创建后又被修改"的情况：直接更新 new_files
        // 条目，避免新增 modified 造成重复或数据陈旧。
        {
            let monitor = match self.volumes.get_mut(drive_letter) {
                Some(m) => m,
                None => return,
            };
            if monitor.fid_index.is_none() {
                log::warn!("[USN] No fid_index for {}, skipping", drive_letter);
                return;
            }
            let fid_index = monitor.fid_index.as_ref().unwrap();
            for (fid, new_result) in &updated {
                let fid_u32 = *fid as u32;
                let parent_path = parent_dir_of(&new_result.path);
                let path_id = monitor.path_table.intern(parent_path);
                if new_result.is_directory {
                    monitor.path_table.intern(&new_result.path);
                }
                let modified_entry = FileEntry::new(
                    new_result.name.clone(),
                    path_id,
                    new_result.size,
                    new_result.modified_time,
                    new_result.file_id,
                    new_result.is_directory,
                );
                if let Some(cache) = self.search_cache.as_mut() {
                    if fid_index
                        .binary_search_by_key(&fid_u32, |(id, _)| *id)
                        .is_ok()
                    {
                        // 文件在 base 中存在：用 modified 隐藏旧 base 条目
                        cache.on_base_entry_hidden(fid_u32);
                        cache
                            .delta
                            .modified
                            .insert(fid_u32, (vol_idx, modified_entry));
                    } else if let Some(&idx) = cache.delta.file_id_index.get(&fid_u32) {
                        // 文件此前已加入 delta.new_files：原地更新，避免重复
                        if let Some((_, existing)) = cache.delta.new_files.get_mut(idx) {
                            *existing = modified_entry;
                        }
                    }
                }
            }
        }

        // 记录被删除 fid 的完整路径，用于 Phase 4 区分"重命名"和"创建后删除"
        let mut removed_path_map: std::collections::HashMap<u32, String> =
            std::collections::HashMap::new();
        for (fid, removed_path) in &removed {
            removed_path_map.insert(*fid as u32, removed_path.clone());
        }

        // Phase 2: Deletions - record IDs
        if has_cache {
            let cache = self.search_cache.as_mut().unwrap();
            for (fid, _removed_path) in &removed {
                let fid_u32 = *fid as u32;
                // 增量维护：先检查再 insert（insert 前 fid 可能已被 modified 隐藏）
                cache.on_base_entry_hidden(fid_u32);
                cache.delta.deleted_ids.insert(fid_u32);
                // 同时清理 delta.modified，避免"先修改后删除"导致已删除文件仍出现
                let was_in_modified = cache.delta.modified.remove(&fid_u32).is_some();
                // 增量维护：modified 移除后，检查 fid 是否不再被任何 delta 隐藏
                if was_in_modified {
                    cache.on_base_entry_unhidden(fid_u32);
                }
                // 清除过期的重命名标记。fid 既然被 USN 删除，就不应再被
                // renamed_fids 保护；真正的重命名会在 Phase 4 重新设置。
                // 否则 NTFS 复用 fid 时，旧重命名残留会导致新删除被错误保留。
                cache.delta.renamed_fids.remove(&fid_u32);
            }
        }

        // Phase 3: Additions - intern paths, collect entries
        let mut added_entries: Vec<(u8, FileEntry)> = Vec::new();
        let mut added_path_map: std::collections::HashMap<u32, String> =
            std::collections::HashMap::new();
        {
            let monitor = self.volumes.get_mut(drive_letter).unwrap();
            for search_result in added {
                // 记录新增条目的完整路径，用于与删除路径比对
                added_path_map.insert(search_result.file_id, search_result.path.to_string());
                let parent_path = parent_dir_of(&search_result.path);
                let path_id = monitor.path_table.intern(parent_path);
                if search_result.is_directory {
                    monitor.path_table.intern(&search_result.path);
                }
                added_entries.push((
                    vol_idx,
                    FileEntry::new(
                        search_result.name,
                        path_id,
                        search_result.size,
                        search_result.modified_time,
                        search_result.file_id,
                        search_result.is_directory,
                    ),
                ));
            }
        }

        // Phase 4: Update cache
        if has_cache {
            let cache = self.search_cache.as_mut().unwrap();
            // Add new files to delta (append-only)
            for entry in added_entries {
                let fid = entry.1.file_id;

                // 区分"重命名"与"创建后立刻删除"：
                // 同一 USN batch 中某 fid 既被 added 又被 removed，
                // 且 added 路径与 removed 路径相同，说明文件创建后又被删除
                // （或临时文件生命周期），不应显示在结果中。
                // 只有 added 路径与 removed 路径不同才是重命名，需要保留新条目。
                if let Some(removed_path) = removed_path_map.get(&fid) {
                    if let Some(added_path) = added_path_map.get(&fid) {
                        if removed_path.eq_ignore_ascii_case(added_path) {
                            // 创建后删除：不加入 delta.new_files，
                            // 但保留 deleted_ids 以隐藏 base 中同 fid 的旧条目
                            continue;
                        }
                    }
                }
                // 如果该文件已在 delta.new_files 中（例如 USN 重复上报创建事件），
                // 直接更新已有条目，避免同一文件在结果中出现两次并导致 total 虚高。
                if let Some(&idx) = cache.delta.file_id_index.get(&fid) {
                    if let Some((_, existing)) = cache.delta.new_files.get_mut(idx) {
                        *existing = entry.1;
                    }
                    // 更新后仍要清理可能存在的 stale modified/deleted 状态
                    let was_in_modified = cache.delta.modified.remove(&fid).is_some();
                    if was_in_modified {
                        cache.on_base_entry_unhidden(fid);
                    }
                    if cache.delta.deleted_ids.contains(&fid) {
                        cache.delta.renamed_fids.insert(fid);
                    } else {
                        cache.delta.deleted_ids.remove(&fid);
                    }
                    continue;
                }

                let file_idx = cache.delta.new_files.len();
                // For renames (fid in both removed and added): keep fid in
                // deleted_ids so the old base entry is hidden, but mark it
                // in renamed_fids so the new delta entry is NOT filtered out.
                // For pure additions (fid NOT in deleted_ids): no conflict.
                if cache.delta.deleted_ids.contains(&entry.1.file_id) {
                    cache.delta.renamed_fids.insert(entry.1.file_id);
                } else {
                    // Pure new file: clear any stale deleted/modified state
                    let was_in_deleted = cache.delta.deleted_ids.remove(&entry.1.file_id);
                    if was_in_deleted {
                        cache.on_base_entry_unhidden(entry.1.file_id);
                    }
                }
                let was_in_modified = cache.delta.modified.remove(&entry.1.file_id).is_some();
                if was_in_modified {
                    cache.on_base_entry_unhidden(entry.1.file_id);
                }
                cache.delta.file_id_index.insert(entry.1.file_id, file_idx);
                cache.delta.new_files.push(entry);
            }
            // Rebuild delta.matched from delta.new_files
            let mut new_delta_matched: Vec<(u8, u32)> = Vec::new();
            for (i, (v, f)) in cache.delta.new_files.iter().enumerate() {
                if cache.delta.deleted_ids.contains(&f.file_id)
                    && !cache.delta.renamed_fids.contains(&f.file_id)
                {
                    continue;
                }
                new_delta_matched.push((*v, i as u32));
            }
            cache.delta.matched = new_delta_matched;
            cache.delta.generation += 1;
        }
    }

    /// 估算当前 delta 缓存占用的内存字节数（粗略上限）。
    pub fn delta_memory_bytes(&self) -> usize {
        let Some(cache) = self.search_cache.as_ref() else {
            return 0;
        };
        let new_files_bytes = cache.delta.new_files.len()
            * (std::mem::size_of::<(u8, FileEntry)>() + std::mem::size_of::<(u32, u32)>());
        let deleted_bytes = cache.delta.deleted_ids.len() * std::mem::size_of::<u32>() * 2;
        let modified_bytes = cache.delta.modified.len()
            * (std::mem::size_of::<u32>()
                + std::mem::size_of::<(u8, FileEntry)>()
                + std::mem::size_of::<u32>())
            * 2;
        let renamed_bytes = cache.delta.renamed_fids.len() * std::mem::size_of::<u32>() * 2;
        let matched_bytes = cache.delta.matched.len() * std::mem::size_of::<(u8, u32)>();
        new_files_bytes + deleted_bytes + modified_bytes + renamed_bytes + matched_bytes
    }

    pub fn merge_if_needed(&mut self) {
        const DELTA_MEMORY_THRESHOLD: usize = 50 * 1024 * 1024; // 50 MB
        const DELTA_COUNT_THRESHOLD: usize = 10_000;
        let should_merge = self.search_cache.as_ref().is_some_and(|c| {
            let count =
                c.delta.new_files.len() + c.delta.deleted_ids.len() + c.delta.modified.len();
            let new_files_bytes = c.delta.new_files.len()
                * (std::mem::size_of::<(u8, FileEntry)>() + std::mem::size_of::<(u32, u32)>());
            let deleted_bytes = c.delta.deleted_ids.len() * std::mem::size_of::<u32>() * 2;
            let modified_bytes = c.delta.modified.len()
                * (std::mem::size_of::<u32>()
                    + std::mem::size_of::<(u8, FileEntry)>()
                    + std::mem::size_of::<u32>())
                * 2;
            let renamed_bytes = c.delta.renamed_fids.len() * std::mem::size_of::<u32>() * 2;
            let matched_bytes = c.delta.matched.len() * std::mem::size_of::<(u8, u32)>();
            let memory =
                new_files_bytes + deleted_bytes + modified_bytes + renamed_bytes + matched_bytes;
            count > DELTA_COUNT_THRESHOLD || memory > DELTA_MEMORY_THRESHOLD
        });
        if should_merge {
            let memory_mb = self.delta_memory_bytes() / 1024 / 1024;
            let cache = self.search_cache.as_mut().unwrap();
            log::info!(
                "Merging delta: {} new, {} deleted, {} modified, ~{} MB",
                cache.delta.new_files.len(),
                cache.delta.deleted_ids.len(),
                cache.delta.modified.len(),
                memory_mb
            );
            cache.merge_delta_to_base(&mut self.volumes, &self.vol_names);
        }
    }

    pub fn delta_count(&self) -> usize {
        self.search_cache
            .as_ref()
            .map_or(0, |c| c.delta.new_files.len())
    }

    /// 返回当前索引的内存统计信息（单位：字节），用于排查内存占用。
    pub fn memory_stats(&self) -> (usize, usize, usize, usize) {
        let files_bytes: usize = self
            .volumes
            .values()
            .map(|m| m.files.len() * std::mem::size_of::<FileEntry>())
            .sum();
        let path_table_bytes: usize = self
            .volumes
            .values()
            .map(|m| m.path_table.memory_estimate())
            .sum();
        let fid_index_bytes: usize = self
            .volumes
            .values()
            .map(|m| {
                m.fid_index
                    .as_ref()
                    .map_or(0, |fi| fi.len() * std::mem::size_of::<(u32, u32)>() * 2)
            })
            .sum();
        let delta_bytes = self.delta_memory_bytes();
        (files_bytes, path_table_bytes, fid_index_bytes, delta_bytes)
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

        apply_incremental_to_cache(
            cache,
            &self.volumes,
            &self.vol_names,
            volume_files,
            vol_idx,
            result,
        )
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
        && result
            .index_map
            .iter()
            .enumerate()
            .all(|(i, m)| *m == Some(i));
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
            let query = if cache.query.trim().is_empty() {
                None
            } else {
                Some(crate::search::query::SearchQuery::parse(&cache.query))
            };
            let needs_path = query.as_ref().is_some_and(|q| q.path_filter.is_some());
            let path_table = vol_names
                .get(vol_idx as usize)
                .and_then(|vn| volumes.get(vn))
                .map(|m| &m.path_table);

            for &new_idx in &result.new_file_indices {
                if new_idx >= files.len() {
                    continue;
                }
                let file = &files[new_idx];
                if let Some(ref q) = query {
                    let full_path = if needs_path {
                        path_table
                            .map(|pt| pt.resolve_file_path(file.path_id, &file.name))
                            .unwrap_or_default()
                    } else {
                        CompactString::new("")
                    };
                    if !crate::search::query::SearchQuery::matches_entry(q, file, &full_path) {
                        continue;
                    }
                }
                if cache.files_only && file.is_directory {
                    continue;
                }
                if cache.directories_only && !file.is_directory {
                    continue;
                }
                new_matched.push((vol_idx, new_idx as u32));
            }
        }
    }
    let _added_count = new_matched.len() - added_count_before;

    cache.base.matched = new_matched;
    cache.base.valid_indices = None;
    cache.base.valid_indices_matched_len = 0;
    cache.sorted_cache = None;
    cache.invalidate_base_count();

    cache.base.matched.len()
}

impl VolumeMonitor {
    pub fn new(
        drive_letter: String,
        include_hidden_files: bool,
        include_system_files: bool,
    ) -> Self {
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
            format!("{}\\", self.drive_letter)
        };

        walkdir::WalkDir::new(&path).follow_links(false)
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
                if name.eq_ignore_ascii_case("$Recycle.Bin") {
                    return false;
                }
                if !include_system && name.eq_ignore_ascii_case("System Volume Information") {
                    return false;
                }
                if !include_hidden && name.starts_with('.') {
                    return false;
                }
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
                .map(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64
                })
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

            if count.is_multiple_of(5000) {
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
        // 预分配 50% 容量：大多数文件共享父目录，实际唯一键数远少于文件总数
        let mut path_map: HashMap<(u32, CompactString), usize> =
            HashMap::with_capacity(self.files.len() / 2);
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
                if name.eq_ignore_ascii_case("$Recycle.Bin") {
                    return false;
                }
                if !include_system && name.eq_ignore_ascii_case("System Volume Information") {
                    return false;
                }
                if !include_hidden && name.starts_with('.') {
                    return false;
                }
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
                .map(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64
                })
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
            if total_processed > 0 && total_processed.is_multiple_of(5000) {
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
            drive_letter,
            added,
            updated,
            removed,
            self.files.len()
        );

        Ok(IncrementalResult {
            added,
            updated,
            removed,
            total: self.files.len(),
            index_map,
            new_file_indices,
        })
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
