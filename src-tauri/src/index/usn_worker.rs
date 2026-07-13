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
