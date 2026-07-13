use crate::index::usn_types::{UsnCommand, UsnResponse, UsnState, VolumeState};
use crate::search::SearchResult;
use crossbeam_channel::{Receiver, Sender};
use std::collections::HashMap;
use std::time::UNIX_EPOCH;
use usn_journal_rs::mft::MftEntry;
use usn_journal_rs::volume::Volume;

struct FileIndex {
    files: Vec<SearchResult>,
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
    log::info!("[USN] Full scan starting for drive {}", drive_letter);

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
    let mut resolver = volume.path_resolver_with_cache();

    // Enumerate MFT entries
    let mft = volume.mft();
    let mut files: Vec<SearchResult> = Vec::with_capacity(1_000_000);
    let mut fid_to_index: HashMap<u64, usize> = HashMap::new();

    const RECYCLE_BIN: &str = "$recycle.bin";
    const SYSTEM_VOL_INFO: &str = "system volume information";

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

        // Filter: skip $Recycle.Bin, System Volume Information
        // Check path components to avoid allocation per entry
        let skip = path.components().any(|comp| {
            let s = comp.as_os_str().to_string_lossy();
            let sl = s.to_lowercase();
            sl == RECYCLE_BIN || sl == SYSTEM_VOL_INFO
        });
        if skip {
            continue;
        }

        // Filter: skip hidden files (by attribute)
        if entry.is_hidden() {
            continue;
        }

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

        // Single allocation for name: Box::from directly
        let name: Box<str> = Box::from(entry.file_name.to_string_lossy().as_ref());
        // Single allocation for path: into() on the String
        let path_str: Box<str> = path.to_string_lossy().to_string().into();

        let index = files.len();
        files.push(SearchResult {
            file_id,
            name,
            path: path_str,
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
    let journal_max_size = usn_journal_rs::DEFAULT_JOURNAL_MAX_SIZE;
    let allocation_delta = usn_journal_rs::DEFAULT_JOURNAL_ALLOCATION_DELTA;

    let (journal_id, last_usn) = match journal.query(true) {
        Ok(data) => {
            // Ensure journal is large enough
            if data.maximum_size < journal_max_size {
                if let Err(e) = journal.create_or_update(journal_max_size, allocation_delta) {
                    log::warn!("[USN] Failed to resize journal for {}: {}", drive_letter, e);
                }
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

    // Store file index (move data in, clone only for response)
    file_indices.insert(
        drive_letter,
        FileIndex {
            files,
            fid_to_index: fid_to_index.clone(),
        },
    );

    // Send response using clone from the stored FileIndex
    if let Some(fi) = file_indices.get(&drive_letter) {
        let _ = resp_tx.send(UsnResponse::FullScanResult {
            drive_letter,
            files: fi.files.clone(),
            file_index: fi.fid_to_index.clone(),
            last_usn,
            journal_id,
        });
    }
}

fn handle_poll_changes(
    drive_letter: char,
    volumes: &HashMap<char, Volume>,
    file_indices: &mut HashMap<char, FileIndex>,
    last_usn_map: &mut HashMap<char, i64>,
    _journal_id_map: &HashMap<char, u64>,
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

    let mut resolver = volume.path_resolver_with_cache();
    let mut added: Vec<(SearchResult, u64)> = Vec::new();
    let mut removed: Vec<u64> = Vec::new();
    let mut updated: Vec<(usize, SearchResult)> = Vec::new();
    let mut new_last_usn = last_usn;

    const USN_REASON_FILE_CREATE: u32 = 0x100;
    const USN_REASON_FILE_DELETE: u32 = 0x200;
    const USN_REASON_RENAME_OLD_NAME: u32 = 0x80000;
    const USN_REASON_RENAME_NEW_NAME: u32 = 0x100000;
    const USN_REASON_DATA_OVERWRITE: u32 = 0x01;
    const USN_REASON_BASIC_INFO_CHANGE: u32 = 0x04;

    const RECYCLE_BIN: &str = "$recycle.bin";
    const SYSTEM_VOL_INFO: &str = "system volume information";

    for result in iter {
        let entry = match result {
            Ok(e) => e,
            Err(_) => continue,
        };

        if entry.usn <= last_usn {
            continue;
        }

        new_last_usn = entry.usn.max(new_last_usn);

        let reason = entry.reason;
        let fid = entry.fid;
        let name: Box<str> = Box::from(entry.file_name.to_string_lossy().as_ref());

        if reason & USN_REASON_FILE_DELETE != 0 || reason & USN_REASON_RENAME_OLD_NAME != 0 {
            if let Some(fi) = file_indices.get_mut(&drive_letter) {
                if let Some(&idx) = fi.fid_to_index.get(&fid) {
                    removed.push(fid);
                    fi.fid_to_index.remove(&fid);
                    if idx < fi.files.len() {
                        fi.files[idx].path = "".into();
                    }
                }
            }
        } else if reason & USN_REASON_FILE_CREATE != 0 || reason & USN_REASON_RENAME_NEW_NAME != 0 {
            let path = match resolver.resolve_path(&entry) {
                Some(p) => p,
                None => continue,
            };

            let skip = path.components().any(|comp| {
                let s = comp.as_os_str().to_string_lossy();
                let sl = s.to_lowercase();
                sl == RECYCLE_BIN || sl == SYSTEM_VOL_INFO
            });
            if skip {
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

            let path_str: Box<str> = path.to_string_lossy().to_string().into();
            let search_result = SearchResult {
                file_id: fid,
                name,
                path: path_str,
                size,
                modified_time,
                is_directory,
            };

            if let Some(fi) = file_indices.get_mut(&drive_letter) {
                let idx = fi.files.len();
                fi.fid_to_index.insert(fid, idx);
                added.push((search_result.clone(), fid));
                fi.files.push(search_result);
            }
        } else if reason & USN_REASON_DATA_OVERWRITE != 0
            || reason & USN_REASON_BASIC_INFO_CHANGE != 0
        {
            if let Some(fi) = file_indices.get_mut(&drive_letter) {
                if let Some(&idx) = fi.fid_to_index.get(&fid) {
                    let path = match resolver.resolve_path(&entry) {
                        Some(p) => p,
                        None => continue,
                    };

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

                    let path_str: Box<str> = path.to_string_lossy().to_string().into();
                    let updated_result = SearchResult {
                        file_id: fid,
                        name,
                        path: path_str,
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

    log::debug!(
        "[USN] Poll {}: added={}, removed={}, updated={}",
        drive_letter,
        added.len(),
        removed.len(),
        updated.len()
    );

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
