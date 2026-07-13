# USN Journal Rs Design

## Overview

Replace the current walkdir-based incremental scanning with NTFS USN change journal + MFT enumeration using the `usn-journal-rs` crate. Requires administrator privileges. Target: `usn-journal-rs` git branch.

## Current State

- Initial scan: walkdir traversal per volume (slow, O(N) full tree walk)
- Incremental scan: walkdir + HashMap diff every 5 seconds (CPU intensive)
- Search: linear scan of in-memory `Vec<SearchResult>` per volume
- `enable_usn_journal` config field exists but is dead code

## Target Architecture

```
┌─────────────────────────────────────────────────┐
│  USN Worker Thread (std::thread)                 │
│  - Holds all Volume handles (usn-journal-rs)     │
│  - MFT enumeration (initial index)               │
│  - USN Journal reading (incremental updates)     │
│  - fid→path path resolution                      │
│  - Periodic USN journal change polling           │
└──────────────┬──────────────────────────────────┘
               │ crossbeam channel (Command/Response)
┌──────────────▼──────────────────────────────────┐
│  async/tokio layer                              │
│  - VolumeManager (holds Sender)                  │
│  - SearchCache (in-memory search results)        │
│  - Tauri commands                               │
└─────────────────────────────────────────────────┘
```

## New Files

### `index/usn_types.rs`

Shared types for channel communication:

```rust
pub enum UsnCommand {
    /// Full MFT scan for a volume
    FullScan { drive_letter: char },
    /// Incremental USN journal poll for a volume
    PollChanges { drive_letter: char },
    /// Shutdown the worker
    Shutdown,
}

pub enum UsnResponse {
    /// Full index built from MFT
    FullScanResult {
        drive_letter: char,
        files: Vec<SearchResult>,
        file_index: HashMap<u64, usize>, // fid → index
        last_usn: i64,
        journal_id: u64,
    },
    /// Incremental changes from USN journal
    IncrementalResult {
        drive_letter: char,
        added: Vec<(SearchResult, u64)>,    // (file, fid)
        removed: Vec<u64>,                   // fids
        updated: Vec<(usize, SearchResult)>, // (index, new_data)
        last_usn: i64,
    },
    /// Error
    Error { message: String },
}
```

### `index/usn_worker.rs`

The USN worker thread that owns all `Volume` handles:

```rust
pub struct UsnWorker {
    volumes: HashMap<char, Volume>,           // drive_letter → Volume
    file_indices: HashMap<char, FileIndex>,   // per-volume fid→index mapping
    last_usn: HashMap<char, i64>,            // per-volume last read USN
    journal_ids: HashMap<char, u64>,         // per-volume journal ID
}

struct FileIndex {
    files: Vec<SearchResult>,
    fid_to_index: HashMap<u64, usize>,
}
```

**Key behaviors:**

1. **FullScan**: For a given drive letter:
   - Open `Volume::from_drive_letter(drive_letter)`
   - Get `volume.mft().iter()` to enumerate all MFT entries
   - For each `MftEntry`:
     - Use `volume.path_resolver_with_cache()` to resolve fid → full path
     - Filter by `file_attributes` (hidden, system, `$Recycle.Bin`, `System Volume Information`)
     - Build `SearchResult` with name, path, is_directory (no size/time from MFT)
   - Create USN journal: `volume.journal().create_or_update(max_size, delta)`
   - Record `journal_id` and `next_usn`
   - Save state to `%APPDATA%/Everything/usn_state.json`
   - Send `UsnResponse::FullScanResult` back via channel

2. **PollChanges**: For a given drive letter:
   - Get `volume.journal().iter_with_options(...)` from `last_usn`
   - For each `UsnEntry`:
     - `USN_REASON_FILE_CREATE` → resolve path, stat for size/time, add to index
     - `USN_REASON_FILE_DELETE` → remove from index
     - `USN_REASON_RENAME_OLD_NAME` → mark for removal
     - `USN_REASON_RENAME_NEW_NAME` → add with new path
     - `USN_REASON_DATA_OVERWRITE` / `USN_REASON_BASIC_INFO_CHANGE` → stat and update
   - Update `last_usn`
   - Save state
   - Send `UsnResponse::IncrementalResult` back via channel

3. **Shutdown**: Break the worker loop, close all handles.

**Thread model:**
- Single `std::thread::spawn` running a loop on `Receiver<UsnCommand>`
- All Volume/UsnJournal/Mft operations happen exclusively on this thread
- Uses `usn_journal_rs::volume::Volume` directly (no Send/Sync needed)

## Modified Files

### `index/mod.rs`

Add `UsnIndexManager` that wraps the channel sender:

```rust
pub struct UsnIndexManager {
    cmd_tx: Sender<UsnCommand>,
    resp_rx: Receiver<UsnResponse>,
    // For Tauri async integration: wrap resp_rx in Arc<Mutex<>> 
    // or use tokio::sync::mpsc bridge
}
```

- `full_scan(drive_letter)` → sends `UsnCommand::FullScan`
- `poll_changes(drive_letter)` → sends `UsnCommand::PollChanges`
- `shutdown()` → sends `UsnCommand::Shutdown`

### `index/monitor.rs`

- Remove `VolumeMonitor::scan_incremental()` (walkdir-based incremental)
- Remove `VolumeMonitor::build_walker()` and `VolumeMonitor::process_walker()`
- Keep `VolumeMonitor::scan()` as fallback for non-admin mode
- `VolumeMonitor` stores results received from USN worker via channel

### `main.rs`

- Create channel pair `(cmd_tx, cmd_rx)` and `(resp_tx, resp_rx)`
- Spawn USN worker thread with `cmd_rx`, `resp_tx`, and volume config
- Bridge `resp_rx` to tokio for async Tauri command integration
- In setup callback:
  - If admin: send `FullScan` commands for each volume
  - Spawn polling task that sends `PollChanges` every N seconds
  - If not admin: fall back to existing walkdir behavior

### `Cargo.toml`

Add dependency:
```toml
usn-journal-rs = "0.4"
```

## USN Journal State Persistence

Store at `%APPDATA%/Everything/usn_state.json`:

```json
{
  "volumes": {
    "C": { "journal_id": 12345, "last_usn": 67890 },
    "D": { "journal_id": 67890, "last_usn": 12345 }
  }
}
```

On startup:
1. Load state file
2. For each volume, query journal: `journal.query(false)`
3. If journal_id matches and `last_usn >= lowest_valid_usn` → incremental from saved USN
4. If mismatch or journal deleted → full MFT rebuild

## Fallback Strategy

- If USN journal read fails (journal truncated/deleted): full MFT rebuild
- If not running as admin: fall back to walkdir-based scanning (existing code)
- If `usn-journal-rs` crate error: log error, fall back to walkdir

## File Metadata (size, timestamps)

MFT entries and USN journal entries do NOT include file size or timestamps. To get full metadata:

- After MFT enumeration, batch-stat files using `std::fs::metadata()` for size/mtime
- For USN journal creates/updates, stat individual files
- This is still much faster than walkdir because:
  - MFT enumeration gives us all file IDs and paths without directory traversal
  - We only stat files we care about (not traversing every directory)
  - USN journal tells us exactly which files changed

## Testing

- Unit tests for UsnCommand/UsnResponse serialization
- Integration test: MFT scan on C: drive, verify file count matches walkdir
- Integration test: create/delete files, verify USN journal picks up changes
- Performance benchmark: MFT scan vs walkdir scan time
