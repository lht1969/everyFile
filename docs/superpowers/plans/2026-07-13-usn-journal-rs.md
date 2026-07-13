# USN Journal Rs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace walkdir-based incremental scanning with NTFS USN change journal + MFT enumeration using the `usn-journal-rs` crate for fast incremental updates under administrator privileges.

**Architecture:** A dedicated `std::thread` holds all `Volume` handles from `usn-journal-rs` (which are `!Send+!Sync`). Communication with the async/Tauri layer uses `crossbeam::channel`. MFT enumeration builds the initial index; USN journal polling detects file changes. Fallback to walkdir for non-admin mode.

**Tech Stack:** `usn-journal-rs` 0.4, `crossbeam-channel`, `rusqlite` (existing), `tokio` (existing), `walkdir` (kept as fallback)

**Spec:** `docs/superpowers/specs/2026-07-13-usn-journal-rs-design.md`

---

## File Structure

| Action | File | Responsibility |
|--------|------|---------------|
| Create | `src-tauri/src/index/usn_types.rs` | Channel command/response enums, USN state persistence types |
| Create | `src-tauri/src/index/usn_worker.rs` | USN worker thread: MFT scan, USN journal polling, path resolution |
| Modify | `src-tauri/src/index/mod.rs` | Add `UsnIndexManager` that wraps channel sender |
| Modify | `src-tauri/src/index/monitor.rs` | Accept results from USN worker, remove walkdir incremental fallback path when admin |
| Modify | `src-tauri/src/main.rs` | Spawn USN worker thread, bridge channel to tokio, wire up polling |
| Modify | `src-tauri/Cargo.toml` | Add `usn-journal-rs` and `crossbeam-channel` dependencies |
| Modify | `src-tauri/src/fs/mod.rs` | Keep existing, no changes needed |

---

## Task 1: Add Dependencies and Create Module Declarations

**Files:**
- Modify: `src-tauri/Cargo.toml`
- Modify: `src-tauri/src/index/mod.rs`

- [ ] **Step 1: Add usn-journal-rs and crossbeam-channel to Cargo.toml**

Open `src-tauri/Cargo.toml` and add under `[dependencies]`:

```toml
usn-journal-rs = "0.4"
crossbeam-channel = "0.5"
```

- [ ] **Step 2: Create usn_types.rs with empty module**

Create `src-tauri/src/index/usn_types.rs`:

```rust
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
```

- [ ] **Step 3: Create usn_worker.rs skeleton**

Create `src-tauri/src/index/usn_worker.rs`:

```rust
use crate::index::usn_types::{UsnCommand, UsnResponse};
use crossbeam_channel::{Receiver, Sender};
use std::collections::HashMap;
use usn_journal_rs::volume::Volume;

struct FileIndex {
    files: Vec<crate::search::SearchResult>,
    fid_to_index: HashMap<u64, usize>,
}

pub fn spawn_usn_worker(
    cmd_rx: Receiver<UsnCommand>,
    resp_tx: Sender<UsnResponse>,
) {
    std::thread::Builder::new()
        .name("usn-worker".into())
        .spawn(move || {
            worker_loop(cmd_rx, resp_tx);
        })
        .expect("failed to spawn USN worker thread");
}

fn worker_loop(cmd_rx: Receiver<UsnCommand>, resp_tx: Sender<UsnResponse>) {
    let mut volumes: HashMap<char, Volume> = HashMap::new();
    let mut file_indices: HashMap<char, FileIndex> = HashMap::new();
    let mut last_usn_map: HashMap<char, i64> = HashMap::new();
    let mut journal_id_map: HashMap<char, u64> = HashMap::new();

    loop {
        match cmd_rx.recv() {
            Ok(UsnCommand::FullScan { drive_letter }) => {
                handle_full_scan(
                    drive_letter,
                    &mut volumes,
                    &mut file_indices,
                    &mut last_usn_map,
                    &mut journal_id_map,
                    &resp_tx,
                );
            }
            Ok(UsnCommand::PollChanges { drive_letter }) => {
                handle_poll_changes(
                    drive_letter,
                    &volumes,
                    &mut file_indices,
                    &mut last_usn_map,
                    &journal_id_map,
                    &resp_tx,
                );
            }
            Ok(UsnCommand::Shutdown) | Err(_) => {
                break;
            }
        }
    }
}

fn handle_full_scan(
    drive_letter: char,
    volumes: &mut HashMap<char, Volume>,
    file_indices: &mut HashMap<char, FileIndex>,
    last_usn_map: &mut HashMap<char, i64>,
    journal_id_map: &mut HashMap<char, u64>,
    resp_tx: &Sender<UsnResponse>,
) {
    // TODO: Task 3
}

fn handle_poll_changes(
    drive_letter: char,
    volumes: &HashMap<char, Volume>,
    file_indices: &mut HashMap<char, FileIndex>,
    last_usn_map: &mut HashMap<char, i64>,
    journal_id_map: &HashMap<char, u64>,
    resp_tx: &Sender<UsnResponse>,
) {
    // TODO: Task 4
}
```

- [ ] **Step 4: Register modules in mod.rs**

Replace the entire content of `src-tauri/src/index/mod.rs`:

```rust
pub mod database;
pub mod monitor;
pub mod usn_types;
pub mod usn_worker;

use database::IndexDatabase;
use monitor::VolumeManager;
use std::sync::{Arc, Mutex};

pub struct IndexManager {
    pub database: Arc<Mutex<IndexDatabase>>,
    pub volume_manager: Arc<Mutex<VolumeManager>>,
}

impl IndexManager {
    pub fn new(database: Arc<Mutex<IndexDatabase>>, volume_manager: Arc<Mutex<VolumeManager>>) -> Self {
        Self {
            database,
            volume_manager,
        }
    }

    pub fn search(&self, query: &str) -> Result<Vec<crate::search::SearchResult>, crate::error::AppError> {
        let db = self.database.lock().map_err(|e| crate::error::AppError::IndexError(e.to_string()))?;
        db.search(query)
    }
}
```

- [ ] **Step 5: Verify compilation**

Run: `cargo check` in `src-tauri/`
Expected: compiles with no errors (usn_worker functions are stubs but compile)

- [ ] **Step 6: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/src/index/usn_types.rs src-tauri/src/index/usn_worker.rs src-tauri/src/index/mod.rs
git commit -m "feat: add USN worker skeleton and channel types"
```

---

## Task 2: USN State Persistence

**Files:**
- Create/Modify: `src-tauri/src/index/usn_types.rs` (already created in Task 1)

This task is already complete within Task 1 Step 2 since `UsnState::load()` and `UsnState::save()` were included. Verify they work.

- [ ] **Step 1: Write a quick test in usn_types.rs**

Add at the bottom of `src-tauri/src/index/usn_types.rs`:

```rust
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
        let mut state = UsnState::default();
        state.volumes.insert(
            "C".to_string(),
            VolumeState {
                journal_id: 12345,
                last_usn: 67890,
            },
        );
        state.save();
        let loaded = UsnState::load();
        assert_eq!(loaded.volumes["C"].journal_id, 12345);
        assert_eq!(loaded.volumes["C"].last_usn, 67890);
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test --lib usn_types` in `src-tauri/`
Expected: 2 tests pass

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/index/usn_types.rs
git commit -m "test: add USN state persistence tests"
```

---

## Task 3: MFT Full Scan Implementation

**Files:**
- Modify: `src-tauri/src/index/usn_worker.rs`

- [ ] **Step 1: Implement handle_full_scan**

Replace the stub `handle_full_scan` in `src-tauri/src/index/usn_worker.rs`:

```rust
use crate::index::usn_types::{UsnCommand, UsnResponse, UsnState, VolumeState};
use crate::search::SearchResult;
use crossbeam_channel::{Receiver, Sender};
use std::collections::HashMap;
use std::ffi::OsString;
use std::time::{SystemTime, UNIX_EPOCH};
use usn_journal_rs::mft::MftEntry;
use usn_journal_rs::path::PathResolvableEntry;
use usn_journal_rs::volume::Volume;

// ... (FileIndex struct and spawn_usn_worker and worker_loop from Task 1 remain)

fn handle_full_scan(
    drive_letter: char,
    volumes: &mut HashMap<char, Volume>,
    file_indices: &mut HashMap<char, FileIndex>,
    last_usn_map: &mut HashMap<char, i64>,
    journal_id_map: &mut HashMap<char, u64>,
    resp_tx: &Sender<UsnResponse>,
) {
    log::info!("[USN] Full scan starting for drive {}: ", drive_letter);

    // Open volume
    let volume = match Volume::from_drive_letter(drive_letter) {
        Ok(v) => v,
        Err(e) => {
            let _ = resp_tx.send(UsnResponse::Error {
                message: format!("Failed to open volume {}: {}", drive_letter, e),
            });
            return;
        }
    };

    // Get path resolver with LRU cache
    let resolver = volume.path_resolver_with_cache();

    // Enumerate MFT entries
    let mft = volume.mft();
    let mut files: Vec<SearchResult> = Vec::new();
    let mut fid_to_index: HashMap<u64, usize> = HashMap::new();

    for result in mft.iter() {
        let entry: MftEntry = match result {
            Ok(e) => e,
            Err(_) => continue,
        };

        // Resolve full path
        let path = match resolver.resolve_path(&entry) {
            Some(p) => p,
            None => continue,
        };

        let path_str = path.to_string_lossy().to_string();

        // Filter: skip $Recycle.Bin, System Volume Information
        let path_lower = path_str.to_lowercase();
        if path_lower.contains("$recycle.bin") || path_lower.contains("system volume information") {
            continue;
        }

        // Filter: skip hidden files (by attribute)
        if entry.is_hidden() {
            continue;
        }

        let name = entry.file_name.to_string_lossy().to_string();
        let is_directory = entry.is_dir();
        let file_id = entry.fid;

        // Get file metadata for size and timestamps
        let (size, modified_time) = match std::fs::metadata(&path) {
            Ok(meta) => {
                let size = meta.len();
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                (size, mtime)
            }
            Err(_) => (0, 0),
        };

        let index = files.len();
        files.push(SearchResult {
            file_id,
            name: name.into(),
            path: path_str.into(),
            size,
            modified_time,
            is_directory,
        });
        fid_to_index.insert(file_id, index);
    }

    log::info!(
        "[USN] Full scan complete for drive {}: {} files",
        drive_letter,
        files.len()
    );

    // Create or verify USN journal
    let journal = volume.journal();
    let journal_max_size = 32 * 1024 * 1024; // 32 MB
    let allocation_delta = 8 * 1024 * 1024; // 8 MB

    let (journal_id, last_usn) = match journal.query(true) {
        Ok(data) => {
            // Ensure journal is large enough
            if data.maximum_size < journal_max_size {
                let _ = journal.create_or_update(journal_max_size, allocation_delta);
                if let Ok(new_data) = journal.query(false) {
                    (new_data.journal_id, new_data.next_usn)
                } else {
                    (data.journal_id, data.next_usn)
                }
            } else {
                (data.journal_id, data.next_usn)
            }
        }
        Err(e) => {
            log::warn!("[USN] Failed to query journal for {}: {}", drive_letter, e);
            // Try creating
            match journal.create_or_update(journal_max_size, allocation_delta) {
                Ok(()) => match journal.query(false) {
                    Ok(data) => (data.journal_id, data.next_usn),
                    Err(e2) => {
                        let _ = resp_tx.send(UsnResponse::Error {
                            message: format!("Journal create+query failed for {}: {}", drive_letter, e2),
                        });
                        return;
                    }
                },
                Err(e2) => {
                    let _ = resp_tx.send(UsnResponse::Error {
                        message: format!("Journal create failed for {}: {}", drive_letter, e2),
                    });
                    return;
                }
            }
        }
    };

    // Save state
    last_usn_map.insert(drive_letter, last_usn);
    journal_id_map.insert(drive_letter, journal_id);

    let mut state = UsnState::load();
    state.volumes.insert(
        drive_letter.to_string(),
        VolumeState {
            journal_id,
            last_usn,
        },
    );
    state.save();

    // Store volume handle
    volumes.insert(drive_letter, volume);

    // Store file index
    file_indices.insert(
        drive_letter,
        FileIndex {
            files: files.clone(),
            fid_to_index,
        },
    );

    let _ = resp_tx.send(UsnResponse::FullScanResult {
        drive_letter,
        files,
        file_index: fid_to_index,
        last_usn,
        journal_id,
    });
}
```

- [ ] **Step 2: Verify compilation**

Run: `cargo check` in `src-tauri/`
Expected: compiles (handle_poll_changes is still a stub)

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/index/usn_worker.rs
git commit -m "feat: implement MFT full scan in USN worker"
```

---

## Task 4: USN Journal Incremental Polling

**Files:**
- Modify: `src-tauri/src/index/usn_worker.rs`

- [ ] **Step 1: Implement handle_poll_changes**

Replace the stub `handle_poll_changes` in `src-tauri/src/index/usn_worker.rs`:

```rust
fn handle_poll_changes(
    drive_letter: char,
    volumes: &HashMap<char, Volume>,
    file_indices: &mut HashMap<char, FileIndex>,
    last_usn_map: &mut HashMap<char, i64>,
    journal_id_map: &HashMap<char, u64>,
    resp_tx: &Sender<UsnResponse>,
) {
    let volume = match volumes.get(&drive_letter) {
        Some(v) => v,
        None => {
            let _ = resp_tx.send(UsnResponse::Error {
                message: format!("No volume handle for drive {}", drive_letter),
            });
            return;
        }
    };

    let last_usn = match last_usn_map.get(&drive_letter) {
        Some(&usn) => usn,
        None => {
            let _ = resp_tx.send(UsnResponse::Error {
                message: format!("No last USN recorded for drive {}", drive_letter),
            });
            return;
        }
    };

    let journal = volume.journal();
    let iter = match journal.iter() {
        Ok(i) => i,
        Err(e) => {
            let _ = resp_tx.send(UsnResponse::Error {
                message: format!("Failed to read journal for {}: {}", drive_letter, e),
            });
            return;
        }
    };

    let resolver = volume.path_resolver_with_cache();
    let mut added: Vec<(SearchResult, u64)> = Vec::new();
    let mut removed: Vec<u64> = Vec::new();
    let mut updated: Vec<(usize, SearchResult)> = Vec::new();
    let mut new_last_usn = last_usn;

    // Get file index for this volume
    let file_index = file_indices.get_mut(&drive_letter);

    for result in iter {
        let entry = match result {
            Ok(e) => e,
            Err(_) => continue,
        };

        // Only process entries after our last known USN
        if entry.usn <= last_usn {
            continue;
        }

        new_last_usn = entry.usn.max(new_last_usn);

        let reason = entry.reason;
        let fid = entry.fid;
        let name = entry.file_name.to_string_lossy().to_string();

        // Classify the change
        const USN_REASON_FILE_CREATE: u32 = 0x100;
        const USN_REASON_FILE_DELETE: u32 = 0x200;
        const USN_REASON_RENAME_OLD_NAME: u32 = 0x80000;
        const USN_REASON_RENAME_NEW_NAME: u32 = 0x100000;
        const USN_REASON_DATA_OVERWRITE: u32 = 0x01;
        const USN_REASON_BASIC_INFO_CHANGE: u32 = 0x04;

        if reason & USN_REASON_FILE_DELETE != 0 || reason & USN_REASON_RENAME_OLD_NAME != 0 {
            // File deleted or renamed away - remove
            if let Some(fi) = file_index {
                if let Some(&idx) = fi.fid_to_index.get(&fid) {
                    removed.push(fid);
                    fi.fid_to_index.remove(&fid);
                    // Mark as removed by setting empty path
                    if idx < fi.files.len() {
                        fi.files[idx].path = "".into();
                    }
                }
            }
        } else if reason & USN_REASON_FILE_CREATE != 0 || reason & USN_REASON_RENAME_NEW_NAME != 0 {
            // File created or renamed to - resolve path and add
            if let Some(path) = resolver.resolve_path(&entry) {
                let path_str = path.to_string_lossy().to_string();
                let path_lower = path_str.to_lowercase();
                if path_lower.contains("$recycle.bin")
                    || path_lower.contains("system volume information")
                {
                    continue;
                }

                let is_directory = entry.is_dir();
                let (size, modified_time) = match std::fs::metadata(&path) {
                    Ok(meta) => {
                        let size = meta.len();
                        let mtime = meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        (size, mtime)
                    }
                    Err(_) => (0, 0),
                };

                let search_result = SearchResult {
                    file_id: fid,
                    name: name.into(),
                    path: path_str.into(),
                    size,
                    modified_time,
                    is_directory,
                };

                if let Some(fi) = file_index {
                    let idx = fi.files.len();
                    fi.fid_to_index.insert(fid, idx);
                    fi.files.push(search_result.clone());
                    added.push((search_result, fid));
                }
            }
        } else if reason & USN_REASON_DATA_OVERWRITE != 0 || reason & USN_REASON_BASIC_INFO_CHANGE != 0 {
            // File modified - update metadata
            if let Some(fi) = file_index {
                if let Some(&idx) = fi.fid_to_index.get(&fid) {
                    if let Some(path) = resolver.resolve_path(&entry) {
                        let path_str = path.to_string_lossy().to_string();
                        let (size, modified_time) = match std::fs::metadata(&path) {
                            Ok(meta) => {
                                let size = meta.len();
                                let mtime = meta
                                    .modified()
                                    .ok()
                                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                                    .map(|d| d.as_secs() as i64)
                                    .unwrap_or(0);
                                (size, mtime)
                            }
                            Err(_) => (0, 0),
                        };

                        let updated_result = SearchResult {
                            file_id: fid,
                            name: name.into(),
                            path: path_str.into(),
                            size,
                            modified_time,
                            is_directory: fi.files[idx].is_directory,
                        };
                        if idx < fi.files.len() {
                            fi.files[idx] = updated_result.clone();
                        }
                        updated.push((idx, updated_result));
                    }
                }
            }
        }
    }

    log::debug!(
        "[USN] Poll {}: added={}, removed={}, updated={}",
        drive_letter,
        added.len(),
        removed.len(),
        updated.len()
    );

    // Update last USN and save state
    if new_last_usn > last_usn {
        last_usn_map.insert(drive_letter, new_last_usn);

        let mut state = UsnState::load();
        if let Some(vs) = state.volumes.get_mut(&drive_letter.to_string()) {
            vs.last_usn = new_last_usn;
        }
        state.save();
    }

    let _ = resp_tx.send(UsnResponse::IncrementalResult {
        drive_letter,
        added,
        removed,
        updated,
        last_usn: new_last_usn,
    });
}
```

- [ ] **Step 2: Verify compilation**

Run: `cargo check` in `src-tauri/`
Expected: compiles with no errors

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/index/usn_worker.rs
git commit -m "feat: implement USN journal incremental polling"
```

---

## Task 5: UsnIndexManager - Channel Wrapper for Async Integration

**Files:**
- Modify: `src-tauri/src/index/mod.rs`

- [ ] **Step 1: Add UsnIndexManager to mod.rs**

Add the following to `src-tauri/src/index/mod.rs` after the existing `IndexManager`:

```rust
use crossbeam_channel::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use crate::index::usn_types::{UsnCommand, UsnResponse};

pub struct UsnIndexManager {
    cmd_tx: Sender<UsnCommand>,
    /// Bridge from crossbeam to tokio for async Tauri commands
    resp_tx: mpsc::UnboundedSender<UsnResponse>,
    resp_rx: Arc<Mutex<mpsc::UnboundedReceiver<UsnResponse>>>,
}

impl UsnIndexManager {
    pub fn new() -> Self {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let (resp_tx, resp_rx) = mpsc::unbounded_channel();

        // Spawn the USN worker thread
        crate::index::usn_worker::spawn_usn_worker(cmd_rx, resp_tx.clone());

        Self {
            cmd_tx,
            resp_tx,
            resp_rx: Arc::new(Mutex::new(resp_rx)),
        }
    }

    pub fn full_scan(&self, drive_letter: char) {
        let _ = self.cmd_tx.send(UsnCommand::FullScan { drive_letter });
    }

    pub fn poll_changes(&self, drive_letter: char) {
        let _ = self.cmd_tx.send(UsnCommand::PollChanges { drive_letter });
    }

    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(UsnCommand::Shutdown);
    }

    pub fn take_response_rx(&self) -> Option<mpsc::UnboundedReceiver<UsnResponse>> {
        self.resp_rx.lock().ok().and_then(|mut rx| {
            // This is a bit awkward - we need to take the receiver once
            // In practice, we'll use a different approach
            None
        })
    }
}
```

Wait - the `resp_rx` approach is wrong. Let me fix this. The `mpsc::UnboundedReceiver` is not `Send` in a way that works with `Arc<Mutex>`. Instead, we should bridge differently.

Let me rewrite this properly:

```rust
use crossbeam_channel::{Receiver, Sender};
use std::sync::mpsc as std_mpsc;

use crate::index::usn_types::{UsnCommand, UsnResponse};

pub struct UsnIndexManager {
    cmd_tx: Sender<UsnCommand>,
    /// Channel to receive responses (blocking, for use in sync contexts)
    resp_rx: std_mpsc::Receiver<UsnResponse>,
}

impl UsnIndexManager {
    pub fn new() -> Self {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let (resp_tx, resp_rx) = std_mpsc::channel();

        crate::index::usn_worker::spawn_usn_worker(cmd_rx, resp_tx);

        Self { cmd_tx, resp_rx }
    }

    pub fn full_scan(&self, drive_letter: char) {
        let _ = self.cmd_tx.send(UsnCommand::FullScan { drive_letter });
    }

    pub fn poll_changes(&self, drive_letter: char) {
        let _ = self.cmd_tx.send(UsnCommand::PollChanges { drive_letter });
    }

    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(UsnCommand::Shutdown);
    }

    /// Non-blocking check for responses
    pub fn try_recv(&self) -> Option<UsnResponse> {
        self.resp_rx.try_recv().ok()
    }

    /// Blocking receive
    pub fn recv(&self) -> Result<UsnResponse, std_mpsc::RecvError> {
        self.resp_rx.recv()
    }
}
```

- [ ] **Step 2: Verify compilation**

Run: `cargo check` in `src-tauri/`
Expected: compiles

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/index/mod.rs
git commit -m "feat: add UsnIndexManager channel wrapper"
```

---

## Task 6: Wire Up USN Worker in main.rs

**Files:**
- Modify: `src-tauri/src/main.rs`

- [ ] **Step 1: Add UsnIndexManager to AppState**

In `src-tauri/src/main.rs`, find the `AppState` struct and add the USN manager:

The exact modification depends on the current AppState definition. The key changes:

1. Add `usn_manager: Option<Arc<UsnIndexManager>>` to AppState
2. After the existing VolumeManager setup, create UsnIndexManager if admin
3. In the setup callback, use UsnIndexManager for full scan instead of walkdir
4. Spawn a polling task that calls `poll_changes` periodically

Here are the specific changes needed:

**In AppState struct** (near the top of main.rs):
```rust
struct AppState {
    index_manager: Arc<Mutex<IndexManager>>,
    volume_manager: Arc<Mutex<VolumeManager>>,
    is_searching: Arc<AtomicBool>,
    last_index_update: Arc<Mutex<Instant>>,
    usn_manager: Option<Arc<crate::index::UsnIndexManager>>,
}
```

**In the setup callback**, after scanning volumes, add USN polling:

```rust
// After the existing scan loop, if admin:
if is_admin {
    let usn_manager = Arc::new(crate::index::UsnIndexManager::new());
    app.manage(usn_manager.clone());

    // Trigger full scan for each volume
    for drive in &drives_to_scan {
        usn_manager.full_scan(*drive);
    }

    // Spawn response handler
    let usn_mgr = usn_manager.clone();
    let vm = volume_manager.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            // Non-blocking check for USN responses
            // Note: need to bridge sync->async carefully
        }
    });

    // Spawn polling task
    let usn_mgr2 = usn_manager.clone();
    let config_clone = config.clone();
    tokio::spawn(async move {
        // Wait for initial scan to complete
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        loop {
            let interval = config_clone.read().index_settings.incremental_interval;
            tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
            for drive in &config_clone.read().monitored_volumes {
                usn_mgr2.poll_changes(*drive);
            }
        }
    });
}
```

The exact wiring requires careful integration with the existing setup callback. The key principle: UsnIndexManager.full_scan() replaces the walkdir scan for admin mode, and a polling task calls poll_changes() periodically.

- [ ] **Step 2: Handle USN responses to update VolumeManager**

The USN responses need to update the VolumeManager's in-memory file list. Add a response handling loop that processes `UsnResponse::FullScanResult` and `UsnResponse::IncrementalResult`.

For `FullScanResult`:
- Replace the volume's files with the MFT-scanned files
- Store the file_index for incremental updates

For `IncrementalResult`:
- Apply adds/removes/updates to the volume's files
- Update the search cache

- [ ] **Step 3: Verify compilation**

Run: `cargo check` in `src-tauri/`
Expected: compiles

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/main.rs
git commit -m "feat: wire up USN worker in main.rs setup"
```

---

## Task 7: Update VolumeManager to Accept USN Results

**Files:**
- Modify: `src-tauri/src/index/monitor.rs`

- [ ] **Step 1: Add methods to apply USN results**

Add methods to `VolumeManager` in `src-tauri/src/index/monitor.rs`:

```rust
impl VolumeManager {
    /// Replace a volume's file list with USN MFT scan results
    pub fn apply_full_scan(
        &mut self,
        drive_letter: char,
        files: Vec<SearchResult>,
        file_index: HashMap<u64, usize>,
    ) {
        if let Some(monitor) = self.volumes.get_mut(&drive_letter.to_string()) {
            monitor.files = files;
            monitor.fid_index = Some(file_index);
            monitor.use_usn = true;
        }
    }

    /// Apply incremental USN changes to a volume
    pub fn apply_incremental_usn(
        &mut self,
        drive_letter: char,
        added: Vec<(SearchResult, u64)>,
        removed: Vec<u64>,
        updated: Vec<(usize, SearchResult)>,
    ) {
        if let Some(monitor) = self.volumes.get_mut(&drive_letter.to_string()) {
            // Apply removals
            for fid in &removed {
                if let Some(fi) = &monitor.fid_index {
                    if let Some(&idx) = fi.get(fid) {
                        if idx < monitor.files.len() {
                            monitor.files[idx].path = "".into(); // mark removed
                        }
                    }
                }
            }

            // Apply updates
            for (idx, result) in updated {
                if idx < monitor.files.len() {
                    monitor.files[idx] = result;
                }
            }

            // Apply additions
            for (result, fid) in added {
                let idx = monitor.files.len();
                monitor.files.push(result);
                if let Some(fi) = &mut monitor.fid_index {
                    fi.insert(fid, idx);
                }
            }

            // Compact: remove entries with empty paths
            monitor.compact_files();
        }
    }
}
```

- [ ] **Step 2: Add fid_index and use_usn fields to VolumeMonitor**

In the `VolumeMonitor` struct in `monitor.rs`, add:

```rust
pub struct VolumeMonitor {
    // ... existing fields ...
    pub fid_index: Option<HashMap<u64, usize>>,
    pub use_usn: bool,
}
```

Initialize both in `new()`:
```rust
fid_index: None,
use_usn: false,
```

- [ ] **Step 3: Add compact_files method to VolumeMonitor**

```rust
impl VolumeMonitor {
    /// Remove entries marked as deleted (empty path) and rebuild fid_index
    pub fn compact_files(&mut self) {
        let old_len = self.files.len();
        self.files.retain(|f| !f.path.is_empty());

        // Rebuild fid_index
        if self.use_usn {
            let mut new_index = HashMap::new();
            for (idx, file) in self.files.iter().enumerate() {
                new_index.insert(file.file_id, idx);
            }
            self.fid_index = Some(new_index);
        }

        if self.files.len() != old_len {
            log::debug!(
                "[VolumeMonitor] {}: compacted {} -> {} files",
                self.drive_letter,
                old_len,
                self.files.len()
            );
        }
    }
}
```

- [ ] **Step 4: Modify scan_incremental to skip when use_usn is true**

In `VolumeMonitor::scan_incremental()`, add an early return:

```rust
pub fn scan_incremental(&mut self) -> crate::error::AppResult<IncrementalResult> {
    // Skip walkdir incremental if using USN journal
    if self.use_usn {
        return Ok(IncrementalResult {
            added: Vec::new(),
            updated: Vec::new(),
            removed: Vec::new(),
            total: self.files.len(),
            index_map: Vec::new(),
            new_file_indices: Vec::new(),
        });
    }
    // ... existing walkdir incremental code ...
}
```

- [ ] **Step 5: Verify compilation**

Run: `cargo check` in `src-tauri/`
Expected: compiles

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/index/monitor.rs
git commit -m "feat: add USN result application to VolumeManager"
```

---

## Task 8: Response Handler Bridge in main.rs

**Files:**
- Modify: `src-tauri/src/main.rs`

This task creates the async bridge that receives USN responses and applies them to the VolumeManager.

- [ ] **Step 1: Add response handler task**

In the setup callback of `main.rs`, after spawning the USN manager, add:

```rust
// Response handler: receives USN worker responses and applies to VolumeManager
let vm_for_usn = volume_manager.clone();
let usn_resp_rx = Arc::new(tokio::sync::Mutex::new(usn_manager.resp_rx.take()));

// Actually, we need a different approach. Let's use a std thread to bridge:
let vm_bridge = volume_manager.clone();
let usn_rx = usn_manager.resp_rx_clone(); // need to add this method

std::thread::spawn(move || {
    loop {
        match usn_rx.recv() {
            Ok(response) => {
                match response {
                    UsnResponse::FullScanResult { drive_letter, files, file_index, .. } => {
                        if let Ok(mut vm) = vm_bridge.lock() {
                            vm.apply_full_scan(drive_letter, files, file_index);
                        }
                    }
                    UsnResponse::IncrementalResult { drive_letter, added, removed, updated, .. } => {
                        if let Ok(mut vm) = vm_bridge.lock() {
                            vm.apply_incremental_usn(drive_letter, added, removed, updated);
                        }
                    }
                    UsnResponse::Error { message } => {
                        log::error!("[USN] Worker error: {}", message);
                    }
                }
            }
            Err(_) => break,
        }
    }
});
```

To make this work, add a `resp_rx_clone()` method to `UsnIndexManager` that returns a clone or shared reference to the response receiver. The simplest approach: use `Arc<Mutex<Receiver>>` or use `crossbeam_channel` for responses too (replacing `std::sync::mpsc`).

Recommended: change UsnIndexManager to use `crossbeam_channel` for both directions:

```rust
pub struct UsnIndexManager {
    cmd_tx: Sender<UsnCommand>,
    resp_rx: Receiver<UsnResponse>,
}

impl UsnIndexManager {
    pub fn new() -> Self {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let (resp_tx, resp_rx) = crossbeam_channel::unbounded();
        crate::index::usn_worker::spawn_usn_worker(cmd_rx, resp_tx);
        Self { cmd_tx, resp_rx }
    }

    pub fn resp_rx_clone(&self) -> Receiver<UsnResponse> {
        self.resp_rx.clone()
    }
    // ... other methods unchanged
}
```

Then the bridge thread can use `resp_rx_clone()`.

- [ ] **Step 2: Verify compilation**

Run: `cargo check` in `src-tauri/`
Expected: compiles

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/main.rs src-tauri/src/index/mod.rs
git commit -m "feat: add async bridge for USN worker responses"
```

---

## Task 9: Admin Mode Detection and Fallback

**Files:**
- Modify: `src-tauri/src/main.rs`

- [ ] **Step 1: Use USN for admin, walkdir for non-admin**

In the setup callback, conditionally use USN or walkdir:

```rust
// Existing code determines `is_admin` via fs::is_elevated()
// Existing code determines `drives_to_scan`

if is_admin {
    // USN mode: spawn worker, full scan via MFT
    let usn_manager = Arc::new(crate::index::UsnIndexManager::new());
    app.manage(usn_manager.clone());

    // Start response handler thread
    // (see Task 8)

    // Trigger full scans
    for &drive in &drives_to_scan {
        usn_manager.full_scan(drive);
    }

    // Start polling task (after delay for initial scan)
    // (see Task 6)
} else {
    // Non-admin mode: use existing walkdir approach
    for drive in &drives_to_scan {
        match monitor.scan_with_progress_callback(*drive, &app_handle) {
            Ok(()) => { /* ... */ }
            Err(e) => { /* ... */ }
        }
    }
}
```

- [ ] **Step 2: Ensure incremental loop skips USN volumes**

In the existing incremental update loop, check `use_usn` on each volume monitor before doing walkdir incremental:

```rust
// In the incremental loop:
for (drive, monitor) in &volume_manager.volumes {
    if monitor.use_usn {
        continue; // USN worker handles this
    }
    // ... existing walkdir incremental ...
}
```

- [ ] **Step 3: Verify compilation**

Run: `cargo check` in `src-tauri/`
Expected: compiles

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/main.rs
git commit -m "feat: conditional USN/walkdir based on admin privileges"
```

---

## Task 10: Remove Dead Code and Clean Up

**Files:**
- Modify: `src-tauri/src/index/monitor.rs`
- Modify: `src-tauri/src/index/usn_worker.rs`

- [ ] **Step 1: Remove unused walkdir imports if any are now dead**

Check if `walkdir` import in `monitor.rs` is still needed (it is, for non-admin fallback). Keep it.

- [ ] **Step 2: Clean up usn_worker.rs imports**

Remove any unused imports and ensure all warnings are resolved.

- [ ] **Step 3: Verify full compilation and run clippy**

Run: `cargo clippy -- -D warnings` in `src-tauri/`
Expected: no warnings

- [ ] **Step 4: Commit**

```bash
git add -A src-tauri/src/
git commit -m "chore: clean up USN worker code and fix warnings"
```

---

## Task 11: Integration Test - Manual Verification

- [ ] **Step 1: Build the application**

Run: `cargo build` in `src-tauri/`
Expected: successful build

- [ ] **Step 2: Run as administrator and verify MFT scan**

1. Run the app as administrator
2. Check logs for `[USN] Full scan complete for drive C: XXXX files`
3. Verify search works and returns results
4. Create a new file in Explorer, wait for polling interval, verify it appears in search
5. Delete a file, wait, verify it disappears from search

- [ ] **Step 3: Run without admin and verify walkdir fallback**

1. Run the app without admin privileges
2. Verify walkdir scan still works
3. Verify incremental updates still work via walkdir

- [ ] **Step 4: Commit final state**

```bash
git add -A
git commit -m "feat: USN journal incremental update via usn-journal-rs"
```
