use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Command sent to the USN worker thread
pub enum UsnCommand {
    /// Full MFT scan for a volume
    FullScan { drive_letter: char },
    /// Incremental USN journal poll for a volume
    PollChanges { drive_letter: char },
    /// Shutdown the worker
    Shutdown,
}

/// Response from the USN worker thread
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
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let path = Self::state_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, json);
        }
    }

    fn state_path() -> std::path::PathBuf {
        let appdata = dirs::data_dir()
            .or_else(|| dirs::home_dir())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        appdata.join("Everything").join("usn_state.json")
    }
}
