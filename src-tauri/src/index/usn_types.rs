use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// 引入 FileEntry 和 PathTable 用于全量扫描结果传输
use crate::index::path_table::PathTable;
use crate::search::FileEntry;

/// Command sent to the USN worker thread
#[derive(Debug)]
pub enum UsnCommand {
    /// Full MFT scan for a volume
    FullScan {
        drive_letter: char,
        include_hidden_files: bool,
        include_system_files: bool,
    },
    /// Incremental USN journal poll for a volume
    PollChanges {
        drive_letter: char,
        include_hidden_files: bool,
        include_system_files: bool,
    },
    /// Shutdown the worker
    #[allow(dead_code)]
    Shutdown,
}

/// Response from the USN worker thread
pub enum UsnResponse {
    /// Full index built from MFT
    /// 携带 FileEntry（紧凑存储）和 PathTable（路径前缀压缩表）
    FullScanResult {
        drive_letter: char,
        /// 紧凑存储的文件条目列表（用 path_id 替代完整路径）
        files: Vec<FileEntry>,
        /// 路径前缀压缩表，用于按需解析完整路径
        path_table: PathTable,
        #[allow(dead_code)]
        last_usn: i64,
    },
    /// Incremental changes from USN journal
    IncrementalResult {
        drive_letter: char,
        /// SearchResults for new files
        added: Vec<crate::search::SearchResult>,
        /// (fid, path) of deleted/renamed-away files
        /// path 用于 fid 与 base 不匹配时按路径兜底定位
        removed: Vec<(u64, String)>,
        /// (file_id, new SearchResult) for updated files - fid-based to avoid index drift
        updated: Vec<(u64, crate::search::SearchResult)>,
        #[allow(dead_code)]
        last_usn: i64,
    },
    /// Error
    Error { message: String },
}

/// Persisted USN journal state per volume
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct UsnState {
    pub volumes: HashMap<String, VolumeState>,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct VolumeState {
    pub last_usn: i64,
}

impl UsnState {
    pub fn load() -> Self {
        let path = Self::state_path();
        match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                log::warn!("[USN] Failed to parse {}: {}", path.display(), e);
                Self::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                log::warn!("[USN] Failed to read {}: {}", path.display(), e);
                Self::default()
            }
        }
    }

    pub fn save(&self) {
        let path = Self::state_path();
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                log::warn!("[USN] Failed to create dir {}: {}", parent.display(), e);
                return;
            }
        }
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    log::warn!("[USN] Failed to write {}: {}", path.display(), e);
                }
            }
            Err(e) => {
                log::warn!("[USN] Failed to serialize USN state: {}", e);
            }
        }
    }

    fn state_path() -> std::path::PathBuf {
        Self::state_path_inner().unwrap_or_else(|| std::path::PathBuf::from("usn_state.json"))
    }

    fn state_path_inner() -> Option<std::path::PathBuf> {
        let appdata = dirs::data_dir().or_else(dirs::home_dir)?;
        Some(appdata.join("Everything").join("usn_state.json"))
    }

    #[cfg(test)]
    fn test_state_path(suffix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("everything_test_usn_{}.json", suffix))
    }

    #[cfg(test)]
    pub fn load_test(suffix: &str) -> Self {
        let path = Self::test_state_path(suffix);
        match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    #[cfg(test)]
    pub fn save_test(&self, suffix: &str) {
        let path = Self::test_state_path(suffix);
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_usn_state_default() {
        let state = UsnState::default();
        assert!(state.volumes.is_empty());
    }

    #[test]
    fn test_usn_state_save_load() {
        let suffix = "save_load";
        let _ = std::fs::remove_file(UsnState::test_state_path(suffix));
        let mut state = UsnState::default();
        state
            .volumes
            .insert("C".to_string(), VolumeState { last_usn: 67890 });
        state.save_test(suffix);
        let loaded = UsnState::load_test(suffix);
        assert_eq!(loaded.volumes["C"].last_usn, 67890);
        let _ = std::fs::remove_file(UsnState::test_state_path(suffix));
    }

    #[test]
    fn test_usn_state_multiple_volumes() {
        let suffix = "multi_vol";
        let _ = std::fs::remove_file(UsnState::test_state_path(suffix));
        let mut state = UsnState::default();
        state
            .volumes
            .insert("C".to_string(), VolumeState { last_usn: 200 });
        state
            .volumes
            .insert("D".to_string(), VolumeState { last_usn: 400 });
        state.save_test(suffix);
        let loaded = UsnState::load_test(suffix);
        assert_eq!(loaded.volumes.len(), 2);
        assert_eq!(loaded.volumes["C"].last_usn, 200);
        assert_eq!(loaded.volumes["D"].last_usn, 400);
        let _ = std::fs::remove_file(UsnState::test_state_path(suffix));
    }
}
