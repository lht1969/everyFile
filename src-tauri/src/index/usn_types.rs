use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Command sent to the USN worker thread
#[derive(Debug)]
pub enum UsnCommand {
    /// Full MFT scan for a volume
    FullScan { drive_letter: char },
    /// Incremental USN journal poll for a volume
    PollChanges { drive_letter: char },
    /// Shutdown the worker
    Shutdown,
}

/// Response from the USN worker thread
#[derive(Debug)]
pub enum UsnResponse {
    /// Full index built from MFT
    FullScanResult {
        drive_letter: char,
        files: Vec<crate::search::SearchResult>,
        /// fid → index into files vec
        file_index: HashMap<u64, usize>,
        last_usn: i64,
        journal_id: u64,
    },
    /// Incremental changes from USN journal
    IncrementalResult {
        drive_letter: char,
        /// (SearchResult, fid) for new files
        added: Vec<(crate::search::SearchResult, u64)>,
        /// fids of deleted files
        removed: Vec<u64>,
        /// (index into files vec, new SearchResult) for updated files
        updated: Vec<(usize, crate::search::SearchResult)>,
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
    pub journal_id: u64,
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
        let appdata = dirs::data_dir().or_else(|| dirs::home_dir())?;
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
        state.volumes.insert(
            "C".to_string(),
            VolumeState {
                journal_id: 12345,
                last_usn: 67890,
            },
        );
        state.save_test(suffix);
        let loaded = UsnState::load_test(suffix);
        assert_eq!(loaded.volumes["C"].journal_id, 12345);
        assert_eq!(loaded.volumes["C"].last_usn, 67890);
        let _ = std::fs::remove_file(UsnState::test_state_path(suffix));
    }

    #[test]
    fn test_usn_state_multiple_volumes() {
        let suffix = "multi_vol";
        let _ = std::fs::remove_file(UsnState::test_state_path(suffix));
        let mut state = UsnState::default();
        state.volumes.insert(
            "C".to_string(),
            VolumeState {
                journal_id: 100,
                last_usn: 200,
            },
        );
        state.volumes.insert(
            "D".to_string(),
            VolumeState {
                journal_id: 300,
                last_usn: 400,
            },
        );
        state.save_test(suffix);
        let loaded = UsnState::load_test(suffix);
        assert_eq!(loaded.volumes.len(), 2);
        assert_eq!(loaded.volumes["C"].journal_id, 100);
        assert_eq!(loaded.volumes["D"].last_usn, 400);
        let _ = std::fs::remove_file(UsnState::test_state_path(suffix));
    }
}
