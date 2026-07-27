pub mod lib;
pub mod monitor;
pub mod ntfs_mft;
pub mod path_table;
pub mod usn_types;
pub mod usn_worker;

use crate::index::usn_types::{UsnCommand, UsnResponse};
use crossbeam_channel::{Receiver, Sender};
use std::collections::HashMap;

pub struct UsnIndexManager {
    /// 每个卷独立的命令发送器
    workers: HashMap<char, Sender<UsnCommand>>,
    /// 所有worker共享的响应接收器
    resp_rx: Receiver<UsnResponse>,
    /// 共享的响应发送器（用于创建新worker时传递）
    resp_tx: Sender<UsnResponse>,
}

impl Default for UsnIndexManager {
    fn default() -> Self {
        Self::new()
    }
}

impl UsnIndexManager {
    pub fn new() -> Self {
        let (resp_tx, resp_rx) = crossbeam_channel::unbounded();
        Self {
            workers: HashMap::new(),
            resp_rx,
            resp_tx,
        }
    }

    pub fn add_volume(&mut self, drive_letter: char) {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        crate::index::usn_worker::spawn_usn_worker(cmd_rx, self.resp_tx.clone());
        self.workers.insert(drive_letter, cmd_tx);
    }

    pub fn full_scan(
        &self,
        drive_letter: char,
        include_hidden_files: bool,
        include_system_files: bool,
    ) {
        if let Some(cmd_tx) = self.workers.get(&drive_letter) {
            let _ = cmd_tx.send(UsnCommand::FullScan {
                drive_letter,
                include_hidden_files,
                include_system_files,
            });
        }
    }

    pub fn poll_changes(
        &self,
        drive_letter: char,
        include_hidden_files: bool,
        include_system_files: bool,
    ) {
        if let Some(cmd_tx) = self.workers.get(&drive_letter) {
            let _ = cmd_tx.send(UsnCommand::PollChanges {
                drive_letter,
                include_hidden_files,
                include_system_files,
            });
        }
    }

    pub fn resp_rx_clone(&self) -> Receiver<UsnResponse> {
        self.resp_rx.clone()
    }
}
