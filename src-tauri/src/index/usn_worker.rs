use crate::index::usn_types::{UsnCommand, UsnResponse, UsnState, VolumeState};
use crate::search::SearchResult;
use crossbeam_channel::{Receiver, Sender};
use std::collections::HashMap;
use std::time::UNIX_EPOCH;
use usn_journal_rs::mft::MftEntry;
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
    let mut resolver = volume.path_resolver_with_cache();

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
            fid_to_index: fid_to_index.clone(),
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

fn handle_poll_changes(
    _drive_letter: char,
    _volumes: &HashMap<char, Volume>,
    _file_indices: &mut HashMap<char, FileIndex>,
    _last_usn_map: &mut HashMap<char, i64>,
    _journal_id_map: &HashMap<char, u64>,
    _resp_tx: &Sender<UsnResponse>,
) {
    // TODO: Task 4
}
