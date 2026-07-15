# Optimize Full Scan Time Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reduce full scan time from ~66s to ~15-20s for 2.2M files on Drive C by eliminating redundant per-file system calls.

**Architecture:** The full scan currently spends 82% of time (~54s) in metadata lookup: for each of 2.2M files it makes a `FSCTL_READ_FILE_USN_DATA` call (for timestamp) + `std::fs::metadata` call (for size). We will:
1. Extract timestamps directly from the MFT enumeration USN records (already returned by `FSCTL_ENUM_USN_DATA`), eliminating 2.2M `FSCTL_READ_FILE_USN_DATA` calls
2. Skip `std::fs::metadata` for directories (size=0, is_dir from file_attributes)
3. Open volume handle once instead of per-chunk
4. Remove diagnostic code

**Tech Stack:** Rust, Windows API (`FSCTL_ENUM_USN_DATA`, `FSCTL_READ_FILE_USN_DATA`), rayon, usn-journal-rs

---

## Current Performance Baseline

Drive C (2,209,604 files):
- MFT Enumeration: ~8s (12%)
- Path Resolution: ~4s (6%)
- Metadata Lookup: ~54s (82%)
- **Total: ~66s**

---

### Task 1: Extract Timestamp from MFT Enumeration Records

**Files:**
- Modify: `C:\Users\lht\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\usn-journal-rs-0.4.1\src\mft.rs` (read-only reference)
- Modify: `D:\projects\everything-tauri\src-tauri\src\index\usn_worker.rs:284-305`
- Modify: `D:\projects\everything-tauri\src-tauri\src\index\usn_worker.rs:402-431`

**Context:** The `FSCTL_ENUM_USN_DATA` API returns USN_RECORD_V2 entries that already contain a `TimeStamp` field at offset +32. Currently the code discards this and later re-reads it via `FSCTL_READ_FILE_USN_DATA` per file. By extracting it during enumeration, we eliminate 2.2M kernel calls.

- [ ] **Step 1: Add `timestamp` field to `RawEntry` struct**

In `usn_worker.rs`, the `RawEntry` struct at line 285 currently has:
```rust
struct RawEntry {
    fid: u64,
    file_name: std::ffi::OsString,
    file_attributes: u32,
}
```

Change to:
```rust
struct RawEntry {
    fid: u64,
    file_name: std::ffi::OsString,
    file_attributes: u32,
    timestamp: i64,
}
```

- [ ] **Step 2: Extract timestamp during MFT enumeration**

The `mft.iter()` returns `MftEntry` which has `usn` (the USN value, not the timestamp). The timestamp is in the raw USN record buffer at offset +32 but is NOT exposed by `MftEntry`. Since we can't modify the external crate, we need to use `FSCTL_ENUM_USN_DATA` directly instead of `mft.iter()`.

Replace the MFT enumeration loop (lines 294-305) with a direct `FSCTL_ENUM_USN_DATA` call that extracts the timestamp. The raw record layout is:
```
+0:  RecordLength (u32)
+8:  FileReferenceNumber (u64)
+16: ParentFileReferenceNumber (u64)
+24: Usn (i64)
+32: TimeStamp (i64) — FILETIME
+40: Reason (u32)
+52: FileAttributes (u32)
+56: FileNameLength (u16)
+58: FileNameOffset (u16)
+60: FileName (UTF-16)
```

Write a helper function `enumerate_mft_entries` that calls `FSCTL_ENUM_USN_DATA` in a loop (similar to `read_usn_records_direct` but using `MFT_ENUM_DATA_V0` input), parses each record to extract fid, parent_fid, file_name, file_attributes, and timestamp. Use a 1MB buffer to reduce kernel calls.

```rust
use windows::Win32::System::Ioctl::MFT_ENUM_DATA_V0;

fn enumerate_mft_entries(
    handle: HANDLE,
) -> Result<(Vec<RawEntry>, HashMap<u64, u64>), String> {
    let mut raw_entries: Vec<RawEntry> = Vec::with_capacity(1_000_000);
    let mut parent_map: HashMap<u64, u64> = HashMap::with_capacity(1_000_000);
    let mut buffer = vec![0u8; 1024 * 1024]; // 1MB buffer
    let mut next_start_fid: u64 = 0;

    loop {
        let enum_data = MFT_ENUM_DATA_V0 {
            StartFileReferenceNumber: next_start_fid,
            LowUsn: 0,
            HighUsn: i64::MAX,
        };

        let mut bytes_returned: u32 = 0;
        let result = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_ENUM_USN_DATA,
                Some(&enum_data as *const _ as _),
                std::mem::size_of::<MFT_ENUM_DATA_V0>() as u32,
                Some(buffer.as_mut_ptr() as *mut _),
                buffer.len() as u32,
                Some(&mut bytes_returned),
                None,
            )
        };

        match result {
            Ok(()) => {
                if bytes_returned as usize <= std::mem::size_of::<u64>() {
                    break;
                }
                next_start_fid = u64::from_le_bytes([
                    buffer[0], buffer[1], buffer[2], buffer[3],
                    buffer[4], buffer[5], buffer[6], buffer[7],
                ]);

                let mut offset = std::mem::size_of::<u64>();
                while offset + 60 <= bytes_returned as usize {
                    let record_length = u32::from_le_bytes([
                        buffer[offset], buffer[offset + 1],
                        buffer[offset + 2], buffer[offset + 3],
                    ]);
                    if record_length == 0 || offset + record_length as usize > bytes_returned as usize {
                        break;
                    }

                    let fid = u64::from_le_bytes(buffer[offset + 8..offset + 16].try_into().unwrap());
                    let parent_fid = u64::from_le_bytes(buffer[offset + 16..offset + 24].try_into().unwrap());
                    let timestamp = i64::from_le_bytes(buffer[offset + 32..offset + 40].try_into().unwrap());
                    let file_attributes = u32::from_le_bytes(buffer[offset + 52..offset + 56].try_into().unwrap());
                    let fn_len = u16::from_le_bytes(buffer[offset + 56..offset + 58].try_into().unwrap()) as usize;
                    let fn_offset = u16::from_le_bytes(buffer[offset + 58..offset + 60].try_into().unwrap()) as usize;

                    let name_bytes = &buffer[offset + fn_offset..offset + fn_offset + fn_len];
                    let file_name = OsString::from_wide(
                        &name_bytes.chunks_exact(2)
                            .map(|c| u16::from_le_bytes([c[0], c[1]]))
                            .collect::<Vec<u16>>()
                    );

                    parent_map.insert(fid, parent_fid);
                    raw_entries.push(RawEntry {
                        fid,
                        file_name,
                        file_attributes,
                        timestamp,
                    });

                    offset += record_length as usize;
                }
            }
            Err(e) => {
                let code = e.code();
                if code == ERROR_HANDLE_EOF.into() {
                    break;
                }
                return Err(format!("FSCTL_ENUM_USN_DATA failed: {}", e));
            }
        }
    }

    Ok((raw_entries, parent_map))
}
```

Then replace lines 279-305 in `handle_full_scan` with:
```rust
let volume_handle = volume.shared_handle();
let (raw_entries, parent_map) = enumerate_mft_entries(volume_handle)
    .map_err(|e| {
        let _ = resp_tx.send(UsnResponse::Error { message: e.clone() });
        e
    })?;
```

Wait — `volume.shared_handle()` returns `Rc<Owned<HANDLE>>`. We need to dereference it. Check how the existing code accesses the handle.

Actually, looking at the existing code, `volume.mft()` returns an `Mft` which holds a reference to the volume. The MFT iterator accesses the handle via `self.volume.shared_handle()`. Since we're bypassing the MFT iterator, we need the raw handle.

The Volume struct likely has a method to get the raw handle. Check `volume.rs` in usn-journal-rs. If not, we can keep using `mft.iter()` and extract timestamps from the raw buffer in a different way.

**Alternative simpler approach:** Since we can't easily modify the external crate, and the `MftEntry` doesn't expose the timestamp, we can instead use a two-pass approach:
- Keep the existing `mft.iter()` for enumeration
- BUT skip the `FSCTL_READ_FILE_USN_DATA` calls entirely
- Get the timestamp from `std::fs::metadata` (which we're already calling for size)

This eliminates one of the two system calls per file. The `std::fs::metadata` call already returns both size AND timestamp, so the `FSCTL_READ_FILE_USN_DATA` call is redundant.

**Revised Step 2:** Modify `get_file_metadata` to skip `FSCTL_READ_FILE_USN_DATA` and use only `std::fs::metadata` for both timestamp and size. This is simpler and still eliminates ~2.2M kernel calls.

Actually, the simplest and most impactful change: modify `UsnMetadataReader::get_file_metadata` to NOT call `read_usn_timestamp` at all, and instead get the timestamp from `std::fs::metadata`. This eliminates the `FSCTL_READ_FILE_USN_DATA` call entirely.

But wait — `std::fs::metadata` on Windows uses `GetFileAttributesExW` which returns `WIN32_FILE_ATTRIBUTE_DATA` including `ftLastWriteTime`. So we can get the timestamp from there.

Let me revise the plan to use this simpler approach.

- [ ] **Step 2 (Revised): Simplify `get_file_metadata` to use only `std::fs::metadata`**

In `ntfs_mft.rs`, change `get_file_metadata` to:
```rust
pub fn get_file_metadata(
    &mut self,
    _fid: u64,
    path: &std::path::Path,
) -> FileMetadata {
    match std::fs::metadata(path) {
        Ok(m) => {
            let modified_time = m.modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            FileMetadata {
                size: m.len(),
                modified_time,
                is_directory: m.is_dir(),
            }
        }
        Err(_) => FileMetadata { size: 0, modified_time: 0, is_directory: false },
    }
}
```

This eliminates the `FSCTL_READ_FILE_USN_DATA` call (and the buffer, handle, and parsing overhead) for every file. The `std::fs::metadata` call already provides everything we need.

- [ ] **Step 3: Verify the change compiles**

Run: `cargo check --manifest-path src-tauri\Cargo.toml`

- [ ] **Step 4: Benchmark before/after**

Time the full scan for Drive C before and after this change. Expected improvement: ~20-30s (eliminating 2.2M `DeviceIoControl` calls).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/index/ntfs_mft.rs
git commit -m "perf: eliminate FSCTL_READ_FILE_USN_DATA calls from full scan

std::fs::metadata already provides timestamp and size. Removing the
separate DeviceIoControl call eliminates ~2.2M kernel transitions per
full scan, reducing metadata lookup time by ~50%."
```

---

### Task 2: Skip Metadata for Directories

**Files:**
- Modify: `D:\projects\everything-tauri\src-tauri\src\index\usn_worker.rs:402-431`

**Context:** ~30% of MFT entries are directories. We already know `is_dir` from `file_attributes` in the MFT entry. For directories, we don't need to call `std::fs::metadata` at all — size is always 0 and is_dir is already known.

- [ ] **Step 1: Add `is_dir` to the entries tuple before metadata lookup**

The entries are already `(u64, Box<str>, bool)` where the bool is `is_dir`. Currently the metadata lookup calls `std::fs::metadata` for every entry including directories.

Change the metadata lookup (lines 402-431) to skip `std::fs::metadata` for directories:

```rust
let files: Vec<SearchResult> = entries_with_path
    .par_chunks(4096)
    .flat_map(|chunk| {
        let mut results = Vec::with_capacity(chunk.len());
        for (fid, path_str, is_dir, path_buf) in chunk {
            if *is_dir {
                let name: Box<str> = name_map.get(fid)
                    .map(|n| Box::from(n.to_string_lossy().as_ref()))
                    .unwrap_or_default();
                results.push(SearchResult {
                    file_id: *fid,
                    name,
                    path: path_str.clone(),
                    size: 0,
                    modified_time: 0,
                    is_directory: true,
                });
                continue;
            }
            let mut reader = ntfs_mft::open_volume_handle(drive_letter)
                .map(|h| ntfs_mft::UsnMetadataReader::new(h));
            let meta = reader
                .as_mut()
                .map(|r| r.get_file_metadata(*fid, path_buf))
                .unwrap_or(ntfs_mft::FileMetadata {
                    size: 0,
                    modified_time: 0,
                    is_directory: false,
                });
            let name: Box<str> = name_map.get(fid)
                .map(|n| Box::from(n.to_string_lossy().as_ref()))
                .unwrap_or_default();
            results.push(SearchResult {
                file_id: *fid,
                name,
                path: path_str.clone(),
                size: meta.size,
                modified_time: meta.modified_time,
                is_directory: meta.is_directory,
            });
        }
        results
    })
    .collect();
```

- [ ] **Step 2: Verify and benchmark**

Run: `cargo check --manifest-path src-tauri\Cargo.toml`
Time the full scan. Expected additional improvement: ~5-10s (skipping ~600K directory metadata calls).

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/index/usn_worker.rs
git commit -m "perf: skip std::fs::metadata for directories in full scan

Directories don't need size/timestamp lookup. Using the is_dir flag
from MFT file_attributes avoids ~600K metadata calls per full scan."
```

---

### Task 3: Open Volume Handle Once for Metadata Chunks

**Files:**
- Modify: `D:\projects\everything-tauri\src-tauri\src\index\usn_worker.rs:402-431`

**Context:** Currently each chunk of 4096 entries opens a NEW volume handle via `ntfs_mft::open_volume_handle`. This creates and destroys ~540 handles for 2.2M files. We should open once and reuse.

- [ ] **Step 1: Open volume handle once before the parallel loop**

Move the volume handle opening outside the `par_chunks` loop. Since `UsnMetadataReader` holds a raw `HANDLE`, we need to be careful about thread safety. `HANDLE` is `Copy`, and the volume handle is opened with `FILE_SHARE_READ | FILE_SHARE_WRITE`, so it's safe to share across threads.

However, `UsnMetadataReader` is not `Sync` because it has a mutable buffer. So we need one reader per thread, but share the handle.

```rust
// Open volume handle once
let vol_handle = ntfs_mft::open_volume_handle(drive_letter);

let files: Vec<SearchResult> = entries_with_path
    .par_chunks(4096)
    .flat_map(|chunk| {
        // Each thread gets its own reader (for the buffer), but shares the handle
        let mut reader = vol_handle.map(|h| ntfs_mft::UsnMetadataReader::new(h));
        // ... rest of chunk processing
    })
    .collect();
```

Wait — `UsnMetadataReader::new` takes ownership of the `HANDLE` (wraps it). But `HANDLE` is `Copy`, so `open_volume_handle` returns `Option<HANDLE>` and we can pass the same handle to multiple readers.

Actually, looking at the code: `UsnMetadataReader` holds `handle: HANDLE`. `HANDLE` is `Copy`. So we can create multiple `UsnMetadataReader` instances with the same handle. Each reader has its own buffer, which is fine for parallel use.

The key change: instead of calling `ntfs_mft::open_volume_handle(drive_letter)` inside the flat_map (which opens a new handle per chunk), call it once outside and pass the handle:

```rust
let vol_handle = ntfs_mft::open_volume_handle(drive_letter);

let files: Vec<SearchResult> = entries_with_path
    .par_chunks(4096)
    .flat_map(|chunk| {
        let mut reader = vol_handle.map(|h| ntfs_mft::UsnMetadataReader::new(h));
        let mut results = Vec::with_capacity(chunk.len());
        for (fid, path_str, is_dir, path_buf) in chunk {
            // ... same as before
        }
        results
    })
    .collect();
```

- [ ] **Step 2: Verify and benchmark**

Run: `cargo check --manifest-path src-tauri\Cargo.toml`

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/index/usn_worker.rs
git commit -m "perf: open volume handle once for metadata lookup

Previously opened a new handle per 4096-entry chunk (~540 handles).
Now opens once and shares across all parallel chunks."
```

---

### Task 4: Remove Diagnostic Code

**Files:**
- Modify: `D:\projects\everything-tauri\src-tauri\src\index\usn_worker.rs:389-400`

**Context:** Lines 389-400 contain diagnostic code that tests the first 3 files and logs their metadata. This adds unnecessary overhead and log noise.

- [ ] **Step 1: Remove the diagnostic block**

Delete lines 389-400:
```rust
// Diagnostic: test first 3 files to verify FSCTL_READ_FILE_USN_DATA returns valid timestamps
if let Some(mut reader) = ntfs_mft::open_volume_handle(drive_letter).map(|h| ntfs_mft::UsnMetadataReader::new(h)) {
    for (fid, path_str, is_dir, path_buf) in entries_with_path.iter().take(3) {
        let meta = reader.get_file_metadata(*fid, path_buf);
        log::info!(
            "[USN-DIAG] fid={} path={} size={} modified_time={} is_dir={}",
            fid, path_str, meta.size, meta.modified_time, meta.is_directory
        );
    }
} else {
    log::info!("[USN-DIAG] FAILED to open volume handle for {}", drive_letter);
}
```

- [ ] **Step 2: Commit**

```bash
git add src-tauri/src/index/usn_worker.rs
git commit -m "chore: remove diagnostic metadata logging from full scan"
```

---

### Task 5: Optimize Path Resolution String Allocations

**Files:**
- Modify: `D:\projects\everything-tauri\src-tauri\src\index\usn_worker.rs:316-376`

**Context:** Path resolution creates a `Vec<OsString>` for each file, pushes path parts, reverses the vec, then formats into a string. For 2.2M files this generates millions of small allocations. We can optimize by:
1. Pre-allocating the parts vec with a reasonable capacity
2. Building the path string in a single pass using a stack buffer

- [ ] **Step 1: Optimize path resolution to reduce allocations**

Replace the path resolution closure in `par_iter` (lines 332-362) with a version that:
- Pre-allocates `parts` with capacity 8 (typical depth)
- Uses a reusable thread-local buffer for path building

```rust
// Inside the par_iter closure:
let mut parts: Vec<std::ffi::OsString> = Vec::with_capacity(8);
parts.push(re.file_name.clone());
let mut cur_fid = re.fid;
for _ in 0..50 {
    match parent_map.get(&cur_fid) {
        Some(&pfid) if pfid != cur_fid && pfid != 0 => {
            cur_fid = pfid;
            if let Some(parent_name) = name_map.get(&pfid) {
                parts.push(parent_name.clone());
            } else {
                break;
            }
        }
        _ => break,
    }
}
parts.reverse();

// Build path string with pre-estimated capacity
let path_str: Box<str> = {
    let estimated_len = parts.iter().map(|p| p.len() + 1).sum::<usize>() + 3; // "C:\" + parts
    let mut path = String::with_capacity(estimated_len);
    path.push_str(drive_letter_str);
    path.push(':');
    for (i, part) in parts.iter().enumerate() {
        path.push('\\');
        path.push_str(&part.to_string_lossy());
    }
    path.into()
};
```

This is a minor optimization but reduces allocation pressure for 2.2M files.

- [ ] **Step 2: Commit**

```bash
git add src-tauri/src/index/usn_worker.rs
git commit -m "perf: reduce allocations in path resolution

Pre-allocate parts vec and estimate path string capacity to reduce
heap allocations during parallel path resolution of 2.2M+ files."
```

---

### Task 6: Final Benchmark and Verification

- [ ] **Step 1: Full build**

```bash
cargo build --manifest-path src-tauri\Cargo.toml --release
```

- [ ] **Step 2: Run the app and time the full scan**

Launch `cargo tauri dev`, check the log for full scan timing:
```
[USN] Full scan starting for drive C
[USN] Enumerated X MFT entries for C
[USN] Resolved Y file paths for C
[USN] Full scan complete for drive C: Z files
```

Record the time delta between "Full scan starting" and "Full scan complete".

- [ ] **Step 3: Verify search works correctly**

Search for various file types (.txt, .lnk, .exe, etc.) to ensure the index is complete.

- [ ] **Step 4: Verify incremental updates still work**

Create, rename, and delete files, then verify they appear/disappear in search results within 60s.

---

## Expected Performance After Optimization

| Phase | Before | After | Savings |
|-------|--------|-------|---------|
| MFT Enumeration | 8s | 8s | 0s |
| Path Resolution | 4s | 3s | 1s |
| Metadata Lookup | 54s | ~12s | ~42s |
| **Total** | **66s** | **~23s** | **~43s (65%)** |

Key wins:
- Eliminating `FSCTL_READ_FILE_USN_DATA` × 2.2M calls (~30s saved)
- Skipping metadata for ~600K directories (~8s saved)
- Reduced allocations and handle churn (~4s saved)
