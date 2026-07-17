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
const MAX_SORT_PERMUTATIONS: usize = 4;

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

pub struct SearchCache {
    /// (query, files_only, directories_only) 三元组 hash
    /// 用于 search_with_options 复用旧 cache，避免 sort 切换时重建整个 search_cache
    cache_key: u64,
    query: String,
    files_only: bool,
    directories_only: bool,
    pub total: usize,
    pub created_at: Instant,
    pub matched: Vec<(u8, u32)>,
    // 排序排列向量：使用 u32 索引而非 usize 以节省内存
    // 221 万文件完全可用 u32 表示，每项从 8 字节降至 4 字节，节省 50%
    sorted_by_name: Option<Vec<u32>>,
    sorted_by_path: Option<Vec<u32>>,
    sorted_by_size: Option<Vec<u32>>,
    sorted_by_modified: Option<Vec<u32>>,
    // LRU tracking: order of last access for each sort permutation (most recent last)
    sort_access_order: Vec<SortBy>,
}

impl SearchCache {
    pub fn is_valid(&self) -> bool {
        self.created_at.elapsed() < Duration::from_secs(CACHE_TTL_SECS)
    }

    pub fn refresh(&mut self) {
        self.created_at = Instant::now();
    }

    /// Evict the least recently used sort permutation if we have too many cached.
    fn evict_lru_permutation(&mut self) {
        if self.sort_access_order.len() <= MAX_SORT_PERMUTATIONS {
            return;
        }
        // The first element in access_order is the least recently used
        let lru = self.sort_access_order.remove(0);
        match lru {
            SortBy::Name | SortBy::Score => { self.sorted_by_name = None; }
            SortBy::Path => { self.sorted_by_path = None; }
            SortBy::Size => { self.sorted_by_size = None; }
            SortBy::ModifiedTime => { self.sorted_by_modified = None; }
        }
        log::info!("Evicted LRU sort permutation: {:?}", lru);
    }

    /// Record that a sort permutation was accessed (move to end of access order).
    fn touch_sort_permutation(&mut self, sort_by: SortBy) {
        // Remove existing entry if present
        self.sort_access_order.retain(|&s| s != sort_by);
        // Add to end (most recent)
        self.sort_access_order.push(sort_by);
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
        // Check if we need to build the permutation
        let needs_build = match sort_by {
            SortBy::Name | SortBy::Score => self.sorted_by_name.is_none(),
            SortBy::Path => self.sorted_by_path.is_none(),
            SortBy::Size => self.sorted_by_size.is_none(),
            SortBy::ModifiedTime => self.sorted_by_modified.is_none(),
        };
        
        if needs_build {
            // Evict LRU before allocating a new permutation
            self.evict_lru_permutation();
            let t0 = Instant::now();
            let matched = &self.matched;
            let perm = build_sort_permutation(matched, volumes, vol_names, sort_by);
            log::info!("build_sort_permutation({:?}): {:?}", sort_by, t0.elapsed());
            match sort_by {
                SortBy::Name | SortBy::Score => { self.sorted_by_name = Some(perm); }
                SortBy::Path => { self.sorted_by_path = Some(perm); }
                SortBy::Size => { self.sorted_by_size = Some(perm); }
                SortBy::ModifiedTime => { self.sorted_by_modified = Some(perm); }
            }
        }
        self.touch_sort_permutation(sort_by);
        
        let matched = &self.matched;
        let indices = match sort_by {
            SortBy::Name | SortBy::Score => self.sorted_by_name.as_ref().unwrap(),
            SortBy::Path => self.sorted_by_path.as_ref().unwrap(),
            SortBy::Size => self.sorted_by_size.as_ref().unwrap(),
            SortBy::ModifiedTime => self.sorted_by_modified.as_ref().unwrap(),
        };
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

        let iter: Box<dyn Iterator<Item = &u32>> = match sort_direction {
            SortDirection::Ascending => Box::new(indices[range_start..range_end].iter()),
            SortDirection::Descending => Box::new(indices[range_start..range_end].iter().rev()),
        };

        iter.filter_map(|idx| {
            let (vol, file_idx) = &matched[*idx as usize];
            let vol_name = &vol_names[*vol as usize];
            // 将内部 FileEntry 转换为对外的 SearchResult
            // 通过 path_table 解析完整路径（FileEntry 不存储完整路径字符串）
            volumes.get(vol_name).and_then(|m| {
                m.files.get(*file_idx as usize).map(|f| {
                    // 文件路径 = 父目录路径 + "\" + 文件名
                    let full_path = m.path_table.resolve_file_path(f.path_id, &f.name);
                    f.to_search_result(full_path)
                })
            })
        }).collect()
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

        // === Cache 复用优化 ===
        // 计算 cache_key：(query, files_only, directories_only) 三元组 hash
        // 如果与旧 cache 相同，说明 matched 内容必然相同（files Vec 未变），
        // 直接复用旧 matched 和已有 perm，**避免每次 sort 都重建整个 search_cache**。
        // 这把"切换排序字段"的耗时从 ~2s（重建 perm）降至 ~50ms（仅 path 解析 first_batch）
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        query.hash(&mut hasher);
        options.files_only.hash(&mut hasher);
        options.directories_only.hash(&mut hasher);
        let new_cache_key = hasher.finish();

        // 检查是否可复用旧 cache
        let can_reuse_cache = if let Some(old) = self.search_cache.as_ref() {
            old.is_valid() && old.cache_key == new_cache_key
        } else {
            false
        };

        // === 取出旧 cache：分离 matched 和 sort perm ===
        // 当 can_reuse_cache 时，matched 和 perm 都有效（matched 向量相同）。
        // 当 can_reuse_cache=false 时，matched 向量已变，旧 perm 索引越界不可复用，
        // 必须丢弃并在下面重建。否则 release 模式下 panic=abort 会直接终止进程。
        let (matched, total, old_perms) = if can_reuse_cache {
            log::info!("search_with_options: reusing cache (key={})", new_cache_key);
            let old = self.search_cache.take().unwrap();
            let m = old.matched;
            let t = old.total;
            // 保留旧排序排列（matched 相同，perm 索引仍然有效）
            let perms = SearchCache {
                cache_key: old.cache_key,
                query: old.query,
                files_only: old.files_only,
                directories_only: old.directories_only,
                total: old.total,
                created_at: old.created_at,
                matched: Vec::new(),
                sorted_by_name: old.sorted_by_name,
                sorted_by_path: old.sorted_by_path,
                sorted_by_size: old.sorted_by_size,
                sorted_by_modified: old.sorted_by_modified,
                sort_access_order: old.sort_access_order,
            };
            (m, t, Some(perms))
        } else {
            // 丢弃旧 cache（其 perm 索引的是旧 matched，不可复用）
            self.search_cache = None;
            // 正常搜索流程
            let total_files: usize = self.volumes.values().map(|v| v.files.len()).sum();
            // 使用 Mutex 保护 Vec，允许多线程并发 push
            // 预估容量 1/4 命中率，过小时 rayon 会按需扩展
            let matched_lock = std::sync::Mutex::new(Vec::with_capacity(total_files / 4));
            let is_empty_query = query.trim().is_empty();
            let parsed_query = if is_empty_query {
                None
            } else {
                Some(crate::search::query::SearchQuery::parse(query))
            };
            let query_controls_dir = parsed_query.as_ref().map_or(false, |q| q.path_filter_dir_only);
            // 仅当查询含 path_filter 时才需要解析完整路径，避免无谓的字符串分配
            let needs_path = parsed_query.as_ref().map_or(false, |q| q.path_filter.is_some());
            let files_only = options.files_only;
            let directories_only = options.directories_only;

            // 并行遍历所有卷的文件
            // 关键优化：把单线程的 221万次 matches_entry 调用分摊到所有 CPU 核心
            // 预期收益：8 核 CPU 上搜索阶段耗时从 ~700ms 降至 ~150ms
            self.volumes.par_iter().for_each(|(vol_key, monitor)| {
                let vol_idx = self.volume_index[vol_key];
                if is_empty_query {
                    // 空查询路径：仅按 files_only/directories_only 过滤
                    let local: Vec<(u8, u32)> = monitor.files.par_iter().enumerate()
                        .filter_map(|(idx, file)| {
                            if files_only && file.is_directory { return None; }
                            if directories_only && !file.is_directory { return None; }
                            Some((vol_idx, idx as u32))
                        })
                        .collect();
                    matched_lock.lock().unwrap().extend(local);
                } else {
                    let pq = parsed_query.as_ref().unwrap();
                    // 非空查询：调用 matches_entry 匹配
                    // 分块并行：每个 rayon worker 独立处理一段文件，最后一次性 extend 减少锁竞争
                    let local: Vec<(u8, u32)> = monitor.files.par_iter().enumerate()
                        .filter_map(|(idx, file)| {
                            // 对于有 path_filter 的查询，需要解析完整路径用于匹配
                            // 对于无 path_filter 的查询，传空字符串以跳过路径检查，避免内存分配
                            let full_path = if needs_path {
                                monitor.path_table.resolve_file_path(file.path_id, &file.name)
                            } else {
                                CompactString::new("")
                            };
                            if !crate::search::query::SearchQuery::matches_entry(pq, file, &full_path) { return None; }
                            if !query_controls_dir && files_only && file.is_directory { return None; }
                            if directories_only && !file.is_directory { return None; }
                            Some((vol_idx, idx as u32))
                        })
                        .collect();
                    matched_lock.lock().unwrap().extend(local);
                }
            });

            let m = matched_lock.into_inner().unwrap();
            let t = m.len();
            log::info!("search_with_options: matched {} files, {:?}", t, t0.elapsed());
            (m, t, None)
        };

        // 为当前 sort_by 准备 perm：复用旧 perm 或重新构建
        // 注意：当前 sort_by 的 perm 在 build_sort_permutation 中会构建，
        // 但其他 sort_by 的 perm 会被保留（来自 old_perms）
        let needs_build_current = match options.sort_by {
            SortBy::Name | SortBy::Score => old_perms.as_ref().map_or(true, |p| p.sorted_by_name.is_none()),
            SortBy::Path => old_perms.as_ref().map_or(true, |p| p.sorted_by_path.is_none()),
            SortBy::Size => old_perms.as_ref().map_or(true, |p| p.sorted_by_size.is_none()),
            SortBy::ModifiedTime => old_perms.as_ref().map_or(true, |p| p.sorted_by_modified.is_none()),
        };

        // 预计算当前排序字段的排列（如果需要）
        let default_perm = if needs_build_current {
            build_sort_permutation(&matched, &self.volumes, &self.vol_names, options.sort_by)
        } else {
            // 复用旧 perm
            match options.sort_by {
                SortBy::Name | SortBy::Score => old_perms.as_ref().unwrap().sorted_by_name.clone().unwrap(),
                SortBy::Path => old_perms.as_ref().unwrap().sorted_by_path.clone().unwrap(),
                SortBy::Size => old_perms.as_ref().unwrap().sorted_by_size.clone().unwrap(),
                SortBy::ModifiedTime => old_perms.as_ref().unwrap().sorted_by_modified.clone().unwrap(),
            }
        };

        // first_batch 需从 FileEntry 转换为 SearchResult 以兼容前端接口
        // 注意：必须使用 default_perm（排序后顺序）而非 matched（MFT 顺序），
        // 否则前端首次显示的是 MFT 顺序的结果，与后续 get_records_range 返回的排序结果不一致，
        // 造成"排序后窗口前面的行残留上次排序的结果"的视觉问题。
        // sort_direction 处理：升序取前50，降序取后50的逆序（与 get_sorted_slice 逻辑一致）
        let first_indices: Vec<u32> = match options.sort_direction {
            SortDirection::Ascending => default_perm.iter().take(50).copied().collect(),
            SortDirection::Descending => default_perm.iter().rev().take(50).copied().collect(),
        };
        let first_batch: Vec<SearchResult> = first_indices.iter()
            .filter_map(|&perm_idx| {
                let (vol, idx) = &matched[perm_idx as usize];
                let vol_name = &self.vol_names[*vol as usize];
                self.volumes.get(vol_name).and_then(|m| {
                    m.files.get(*idx as usize).map(|f| {
                        let full_path = m.path_table.resolve_file_path(f.path_id, &f.name);
                        f.to_search_result(full_path)
                    })
                })
            })
            .collect();

        // === 重建 SearchCache，保留旧 perm 缓存 ===
        // 关键：复用 old_perms 中已有的 4 个 sorted_by_* 字段，
        // 仅覆盖当前 sort_by 的字段为新构建/复用的 perm
        let (mut sn, mut sp, mut ss, mut sm) = match options.sort_by {
            SortBy::Name | SortBy::Score => (Some(default_perm), old_perms.as_ref().and_then(|p| p.sorted_by_path.clone()), old_perms.as_ref().and_then(|p| p.sorted_by_size.clone()), old_perms.as_ref().and_then(|p| p.sorted_by_modified.clone())),
            SortBy::Path => (old_perms.as_ref().and_then(|p| p.sorted_by_name.clone()), Some(default_perm), old_perms.as_ref().and_then(|p| p.sorted_by_size.clone()), old_perms.as_ref().and_then(|p| p.sorted_by_modified.clone())),
            SortBy::Size => (old_perms.as_ref().and_then(|p| p.sorted_by_name.clone()), old_perms.as_ref().and_then(|p| p.sorted_by_path.clone()), Some(default_perm), old_perms.as_ref().and_then(|p| p.sorted_by_modified.clone())),
            SortBy::ModifiedTime => (old_perms.as_ref().and_then(|p| p.sorted_by_name.clone()), old_perms.as_ref().and_then(|p| p.sorted_by_path.clone()), old_perms.as_ref().and_then(|p| p.sorted_by_size.clone()), Some(default_perm)),
        };

        // 重建 sort_access_order：保留旧顺序，更新当前 sort_by 到末尾
        let mut sort_access_order = old_perms.as_ref()
            .map(|p| p.sort_access_order.clone())
            .unwrap_or_default();
        sort_access_order.retain(|&s| s != options.sort_by);
        sort_access_order.push(options.sort_by);

        // LRU 淘汰：如果 sort_access_order 超过容量，淘汰最旧的并清空其 perm
        while sort_access_order.len() > MAX_SORT_PERMUTATIONS {
            let lru = sort_access_order.remove(0);
            match lru {
                SortBy::Name | SortBy::Score => { sn = None; }
                SortBy::Path => { sp = None; }
                SortBy::Size => { ss = None; }
                SortBy::ModifiedTime => { sm = None; }
            }
            log::info!("Evicted LRU sort permutation: {:?}", lru);
        }

        self.search_cache = Some(SearchCache {
            cache_key: new_cache_key,
            query: query.to_string(),
            files_only: options.files_only,
            directories_only: options.directories_only,
            matched,
            total,
            created_at: Instant::now(),
            sorted_by_name: sn,
            sorted_by_path: sp,
            sorted_by_size: ss,
            sorted_by_modified: sm,
            sort_access_order,
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
        let total = cache.total;
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
    pub fn apply_incremental_usn(
        &mut self,
        drive_letter: &str,
        added: Vec<SearchResult>,
        removed: Vec<u64>,
        updated: Vec<(u64, SearchResult)>,
    ) {
        let monitor = match self.volumes.get_mut(drive_letter) {
            Some(m) => m,
            None => return,
        };

        if monitor.fid_index.is_none() {
            log::warn!("[USN] No fid_index for {}, skipping incremental update", drive_letter);
            return;
        }

        // 处理更新：通过 fid 查找当前索引（避免压缩后索引漂移）
        // 将 SearchResult 转换为 FileEntry
        for (fid, new_result) in updated {
            let fid_u32 = fid as u32;
            if let Some(idx) = monitor.fid_index.as_ref().and_then(|fi| fi.iter().find(|(id, _)| *id == fid_u32).map(|(_, idx)| *idx)) {
                if (idx as usize) < monitor.files.len() {
                    // 文件：intern 父目录路径得到 path_id（FileEntry.path_id 指向父目录）
                    let parent_path = parent_dir_of(&new_result.path);
                    let path_id = monitor.path_table.intern(parent_path);
                    // 目录：额外注册自身路径供子条目使用
                    if new_result.is_directory {
                        monitor.path_table.intern(&new_result.path);
                    }
                    monitor.files[idx as usize] = FileEntry::new(
                        new_result.name,
                        path_id,
                        new_result.size,
                        new_result.modified_time,
                        new_result.file_id,
                        new_result.is_directory,
                    );
                }
            }
        }

        // 处理删除：通过 fid 映射到索引，标记 path_id 为已删除
        {
            let fid_index = monitor.fid_index.as_ref().unwrap();
            let indices_to_remove: Vec<usize> = removed
                .iter()
                .filter_map(|fid| {
                    let fid_u32 = *fid as u32;
                    fid_index.iter().find(|(id, _)| *id == fid_u32).map(|(_, idx)| *idx as usize)
                })
                .collect();
            for idx in indices_to_remove {
                if idx < monitor.files.len() {
                    // 标记为已删除（替代原来的 path = ""）
                    monitor.files[idx].path_id = PathTable::deleted_id();
                }
            }
        }

        // 处理新增：将 SearchResult 转换为 FileEntry 后追加
        for search_result in added {
            // 文件：intern 父目录路径得到 path_id（FileEntry.path_id 指向父目录）
            let parent_path = parent_dir_of(&search_result.path);
            let path_id = monitor.path_table.intern(parent_path);
            // 目录：额外注册自身路径供子条目使用
            if search_result.is_directory {
                monitor.path_table.intern(&search_result.path);
            }
            let entry = FileEntry::new(
                search_result.name,
                path_id,
                search_result.size,
                search_result.modified_time,
                search_result.file_id,
                search_result.is_directory,
            );
            monitor.files.push(entry);
        }

        // 压缩：移除已删除条目，重建 fid_index
        monitor.compact_files();

        self.search_cache = None;
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

/// 增量更新缓存（自由函数，避免借用冲突）
///
/// 适配 FileEntry：通过 volumes 获取对应卷的 path_table，
/// 用于在含 path_filter 的查询下解析完整路径
fn apply_incremental_to_cache(
    cache: &mut SearchCache,
    volumes: &HashMap<String, VolumeMonitor>,
    vol_names: &[String],
    volume_files: Option<&[FileEntry]>,
    vol_idx: u8,
    result: &IncrementalResult,
) -> usize {
    let mut new_matched: Vec<(u8, u32)> = Vec::with_capacity(cache.matched.len());
    let mut removed_count = 0usize;

    for (vol, idx) in cache.matched.drain(..) {
        if vol != vol_idx {
            new_matched.push((vol, idx));
        } else if (idx as usize) < result.index_map.len() {
            if let Some(new_idx) = result.index_map[idx as usize] {
                new_matched.push((vol, new_idx as u32));
            } else {
                removed_count += 1;
            }
        } else {
            removed_count += 1;
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

            // 仅当查询含 path_filter 时才需要解析完整路径
            let needs_path = query.as_ref().map_or(false, |q| q.path_filter.is_some());
            // 通过 vol_idx 找到对应的 VolumeMonitor，获取其 path_table
            let path_table = volumes.get(&vol_names[vol_idx as usize]).map(|m| &m.path_table);

            for &new_idx in &result.new_file_indices {
                if new_idx >= files.len() { continue; }
                let file = &files[new_idx];

                if let Some(ref q) = query {
                    // 解析完整路径（仅在需要时）
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
    let added_count = new_matched.len() - added_count_before;

    cache.matched = new_matched;
    cache.total = cache.matched.len();

    if removed_count > 0 || added_count > 0 {
        cache.sorted_by_name = None;
        cache.sorted_by_path = None;
        cache.sorted_by_size = None;
        cache.sorted_by_modified = None;
    }

    cache.total
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
    pub fn compact_files(&mut self) {
        // 通过 PathTable::is_deleted 判断是否已删除
        self.files.retain(|f| !PathTable::is_deleted(f.path_id));
        let mut new_fid_index: Vec<(u32, u32)> = Vec::with_capacity(self.files.len());
        for (i, f) in self.files.iter().enumerate() {
            new_fid_index.push((f.file_id, i as u32));
        }
        new_fid_index.sort_unstable_by_key(|(id, _)| *id);
        self.fid_index = Some(new_fid_index);
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