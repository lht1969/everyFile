//! 路径前缀压缩表
//!
//! 将完整路径拆分为 (parent_path_id, dir_name) 的层级结构，
//! 避免为每个文件存储完整路径字符串。
//!
//! 例如 221 万文件共享 30-50 万目录，PathTable 仅存储每个唯一目录一次。
//! 每个文件只需存 path_id: u32 (4字节) 而非完整路径 (平均 100+ 字节)。
//!
//! 内存节省估算：
//! - 原方案：221万 × (路径字符串 ~116字节 + 名称 ~23字节) ≈ 308 MB
//! - 新方案：221万 × 4字节(path_id) + 40万目录 × ~80字节 ≈ 40 MB
//! - 节省约 268 MB

use compact_str::CompactString;
use std::collections::HashMap;

/// 路径条目：parent_path_id + 目录名
/// 根目录的 parent_path_id = 0 (自身)
#[derive(Clone)]
struct PathEntry {
    parent_id: u32,
    name: CompactString,
    /// 字典序编号：所有 path 按完整路径字典序排序后分配的连续编号 (0, 1, 2, ...)
    /// 排序时 (entry_a < entry_b) 当且仅当 (a.ordinal < b.ordinal)
    /// 关键优化：用 u32 整数比较取代字符串比较，Path 排序从 O(strlen) 降至 O(1)
    /// 初始值为 u32::MAX（未计算），compute_ordinals() 后填入正确编号
    ordinal: u32,
}

/// 路径前缀压缩表
///
/// 存储所有唯一目录路径，每个目录分配一个 u32 ID。
/// 通过 parent_id 链式拼接得到完整路径。
pub struct PathTable {
    /// path_id → (parent_id, dir_name)
    entries: Vec<PathEntry>,
    /// 完整路径字符串 → path_id 的映射，用于去重
    /// 仅在插入时使用，查询时不走此路径
    path_to_id: HashMap<CompactString, u32>,
}

/// 特殊 path_id 值
const ROOT_PATH_ID: u32 = 0;
const DELETED_PATH_ID: u32 = u32::MAX;

impl PathTable {
    pub fn new() -> Self {
        let mut table = Self {
            entries: Vec::with_capacity(500_000),
            path_to_id: HashMap::with_capacity(500_000),
        };
        // 占位 entry，使 path_id 从 1 开始（0 保留给"无路径"）
        // ordinal 设为 0（ROOT 是所有 path 的祖先，字典序最小）
        table.entries.push(PathEntry {
            parent_id: 0,
            name: CompactString::new(""),
            ordinal: 0,
        });
        table
    }

    /// 注册一个完整路径，返回其 path_id。
    /// 如果路径已存在则返回已有 id，否则创建新条目。
    ///
    /// 路径格式应为 "C:\Windows\System32" 这样的完整路径。
    /// 首次调用应注册根目录（如 "C:\"）。
    pub fn intern(&mut self, full_path: &str) -> u32 {
        if full_path.is_empty() {
            return DELETED_PATH_ID;
        }

        // 检查是否已注册
        if let Some(&id) = self.path_to_id.get(full_path) {
            return id;
        }

        // 解析路径：找到最后一个反斜杠，分割为 parent + name
        let (parent_id, name) = if let Some(pos) = full_path.rfind('\\') {
            let parent_path = &full_path[..pos];
            let name = &full_path[pos + 1..];
            let pid = if parent_path.is_empty() {
                ROOT_PATH_ID
            } else {
                self.intern(parent_path)
            };
            (pid, name)
        } else {
            // 没有反斜杠，视为根目录（如 "C:"）
            (ROOT_PATH_ID, full_path)
        };

        let new_id = self.entries.len() as u32;
        // ordinal 初始化为 u32::MAX（标记未计算），compute_ordinals() 后填入正确编号
        self.entries.push(PathEntry {
            parent_id,
            name: CompactString::from(name),
            ordinal: u32::MAX,
        });
        self.path_to_id
            .insert(CompactString::from(full_path), new_id);
        new_id
    }

    /// 根据 path_id 解析目录完整路径
    ///
    /// 通过 parent_id 链向上遍历，拼接所有目录名。
    /// 对于深层路径，复杂度为 O(depth)，通常 <20 层。
    ///
    /// 注意：path_id 必须指向一个目录条目。
    /// 若要解析文件路径，请使用 `resolve_file_path(path_id, file_name)`。
    ///
    /// 特殊处理：路径中可能出现空 name（如 "C:\" 的 entry.name=""），
    /// 表示该层是"以 \\ 结尾"的根目录标记。允许空 name 继续向上遍历父链，
    /// 拼接时若遇到空 name 会在 result 末尾追加 \\。
    pub fn resolve_path(&self, path_id: u32) -> CompactString {
        if path_id == ROOT_PATH_ID || path_id == DELETED_PATH_ID {
            return CompactString::new("");
        }

        // 收集路径组件（从深到浅）
        // 关键修复：原本遇到空 name 会 break，导致 C:\ 根目录的 entry（name=""）
        // 解析时直接丢失 C:\ 前缀。改为允许空 name 继续遍历，拼接时再处理
        let mut components: Vec<&CompactString> = Vec::with_capacity(16);
        let mut cur = path_id;
        for _ in 0..64 {
            // 防止循环引用导致的死循环
            if cur == ROOT_PATH_ID || (cur as usize) >= self.entries.len() {
                break;
            }
            let entry = &self.entries[cur as usize];
            // 修复：不再 break 空 name（"C:\" 的 entry.name="" 需要参与拼接）
            components.push(&entry.name);
            cur = entry.parent_id;
        }

        if components.is_empty() {
            return CompactString::new("");
        }

        components.reverse();

        // 拼接逻辑
        // - 第一个 component 直接拼接（通常是 "C:"）
        // - 后续 component：若 result 不以 \\ 结尾则先加 \\，再加 name
        // - 空 name 表示"路径以 \\ 结束"（如 "C:\"），仅添加 \\ 不拼接 name
        let mut result = String::with_capacity(64);
        for comp in components.iter() {
            if comp.is_empty() {
                // 空 name = 路径以 \ 结尾的 marker，仅补齐末尾的 \
                if !result.is_empty() && !result.ends_with('\\') {
                    result.push('\\');
                }
            } else {
                if !result.is_empty() && !result.ends_with('\\') {
                    result.push('\\');
                }
                result.push_str(comp);
            }
        }
        CompactString::from(result)
    }

    /// 解析文件的完整路径
    ///
    /// `path_id` 指向文件所在的父目录，`file_name` 是文件名。
    /// 返回 父目录路径 + "\\" + file_name。
    ///
    /// 这样设计是因为 PathTable 只存储目录路径（~40万），
    /// 文件不注册到 path_to_id，避免为 221万文件存储完整路径字符串。
    /// FileEntry.path_id 指向父目录，配合 FileEntry.name 即可还原完整路径。
    ///
    /// 修复：原实现在 dir 不为空时无条件 push '\\'，
    /// 但对于 C:\ 根目录（dir="C:\"），会导致路径变成 "C:\\file.txt"（两个 \\）。
    /// 改为：若 dir 已以 \\ 结尾则不重复加。
    pub fn resolve_file_path(&self, path_id: u32, file_name: &str) -> CompactString {
        if path_id == ROOT_PATH_ID || path_id == DELETED_PATH_ID {
            return CompactString::from(file_name);
        }
        let dir = self.resolve_path(path_id);
        if dir.is_empty() {
            return CompactString::from(file_name);
        }
        // dir + (可选 "\") + file_name
        let needs_sep = !dir.ends_with('\\');
        let mut result = String::with_capacity(dir.len() + file_name.len() + 1);
        result.push_str(&dir);
        if needs_sep {
            result.push('\\');
        }
        result.push_str(file_name);
        CompactString::from(result)
    }

    /// 清理去重用的 HashMap，释放内存
    ///
    /// 扫描完成后，`path_to_id` 不再需要（后续增量更新可通过 `intern` 重建单个条目）。
    /// 对于 221万文件场景，path_to_id 存储了 ~40万目录路径字符串，
    /// 清理后可释放约 40-100MB 内存。
    ///
    /// 注意：调用此方法后，`intern` 对新路径仍可正常工作（会重建 HashMap 条目），
    /// 但已存在路径的去重查找会失效，导致重复插入。
    /// 因此仅在全量扫描完成且不再有大批量 intern 调用时使用。
    pub fn clear_dedup_map(&mut self) {
        self.path_to_id.clear();
        self.path_to_id.shrink_to_fit();
    }

    /// 为所有 path 分配字典序 ordinal
    ///
    /// 算法：解析所有目录的完整路径字符串，按字符串字典序排序，
    /// 然后分配 0, 1, 2, ... 连续编号（ROOT 已固定为 0）。
    ///
    /// 这保证 ordinal 顺序 = 字典序，Path 排序可直接用 O(1) 整数比较。
    ///
    /// 调用时机：所有 intern 完成后（apply_full_scan 结束）
    /// 复杂度：O(N * depth + N log N)，N = entries.len()
    pub fn compute_ordinals(&mut self) {
        if self.entries.len() <= 1 {
            return;
        }

        let mut sorted_indices: Vec<u32> = (1..self.entries.len() as u32).collect();

        // 解析所有 entry 的完整路径字符串，用于字典序排序
        let resolved: Vec<CompactString> = sorted_indices
            .iter()
            .map(|&id| self.resolve_path(id))
            .collect();

        sorted_indices
            .sort_by(|&a, &b| resolved[(a - 1) as usize].cmp(&resolved[(b - 1) as usize]));

        for (i, &id) in sorted_indices.iter().enumerate() {
            self.entries[id as usize].ordinal = (i + 1) as u32;
        }
    }

    /// 检查指定 path_id 的 ordinal 是否已计算
    #[allow(dead_code)]
    pub fn is_ordinal_computed(&self, path_id: u32) -> bool {
        if path_id == ROOT_PATH_ID || path_id == DELETED_PATH_ID {
            return true;
        }
        if (path_id as usize) >= self.entries.len() {
            return false;
        }
        self.entries[path_id as usize].ordinal != u32::MAX
    }

    /// 获取 path 的字典序编号（用于 Path 排序的 O(1) 比较）
    ///
    /// 调用前必须先调用 compute_ordinals()，否则返回 u32::MAX（未计算）
    /// 对于 ROOT 和 DELETED，返回特殊值（0 和 u32::MAX-1）
    #[inline]
    pub fn get_ordinal(&self, path_id: u32) -> u32 {
        if path_id == ROOT_PATH_ID {
            return 0;
        }
        if path_id == DELETED_PATH_ID {
            return u32::MAX - 1;
        }
        if (path_id as usize) >= self.entries.len() {
            return u32::MAX;
        }
        self.entries[path_id as usize].ordinal
    }

    /// 检查 path_id 是否表示已删除的条目
    #[inline]
    pub fn is_deleted(path_id: u32) -> bool {
        path_id == DELETED_PATH_ID
    }

    /// 获取用于标记删除的 path_id
    #[inline]
    pub fn deleted_id() -> u32 {
        DELETED_PATH_ID
    }

    /// 返回已存储的路径条目数（不含占位）
    pub fn len(&self) -> usize {
        self.entries.len().saturating_sub(1)
    }

    /// 是否为空
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.len() <= 1
    }
}

impl Default for PathTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_intern_and_resolve() {
        let mut table = PathTable::new();

        let id1 = table.intern("C:\\");
        let id2 = table.intern("C:\\Windows");
        let id3 = table.intern("C:\\Windows\\System32");

        assert_eq!(table.resolve_path(id1), "C:\\");
        assert_eq!(table.resolve_path(id2), "C:\\Windows");
        assert_eq!(table.resolve_path(id3), "C:\\Windows\\System32");
    }

    #[test]
    fn test_dedup() {
        let mut table = PathTable::new();

        let id1 = table.intern("C:\\Windows\\System32");
        let id2 = table.intern("C:\\Windows\\System32");

        assert_eq!(id1, id2);
    }

    #[test]
    fn test_shared_parent() {
        let mut table = PathTable::new();

        let id1 = table.intern("C:\\Windows\\System32\\file1.dll");
        let id2 = table.intern("C:\\Windows\\System32\\file2.dll");
        let id3 = table.intern("C:\\Windows\\System32\\subdir");

        // 三个路径共享父目录 "C:\Windows\System32"
        let parent1 = table.entries[id1 as usize].parent_id;
        let parent2 = table.entries[id2 as usize].parent_id;
        let parent3 = table.entries[id3 as usize].parent_id;

        assert_eq!(parent1, parent2);
        assert_eq!(parent2, parent3);
    }

    #[test]
    fn test_empty_path() {
        let mut table = PathTable::new();
        let id = table.intern("");
        assert_eq!(id, DELETED_PATH_ID);
    }

    #[test]
    fn test_deep_path() {
        let mut table = PathTable::new();
        let deep_path = "C:\\a\\b\\c\\d\\e\\f\\g\\h\\i\\j\\file.txt";
        let id = table.intern(deep_path);
        assert_eq!(table.resolve_path(id), deep_path);
    }
}
