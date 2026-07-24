pub mod lib;
pub mod monitor;
pub mod ntfs_mft;
pub mod path_table;
pub mod usn_types;
pub mod usn_worker;

use crate::index::usn_types::{UsnCommand, UsnResponse};
use crossbeam_channel::{Receiver, Sender};

pub struct UsnIndexManager {
    cmd_tx: Sender<UsnCommand>,
    resp_rx: Receiver<UsnResponse>,
}

impl Default for UsnIndexManager {
    fn default() -> Self {
        Self::new()
    }
}

impl UsnIndexManager {
    pub fn new() -> Self {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let (resp_tx, resp_rx) = crossbeam_channel::unbounded();
        crate::index::usn_worker::spawn_usn_worker(cmd_rx, resp_tx);
        Self { cmd_tx, resp_rx }
    }

    pub fn full_scan(
        &self,
        drive_letter: char,
        include_hidden_files: bool,
        include_system_files: bool,
    ) {
        let _ = self.cmd_tx.send(UsnCommand::FullScan {
            drive_letter,
            include_hidden_files,
            include_system_files,
        });
    }

    pub fn poll_changes(
        &self,
        drive_letter: char,
        include_hidden_files: bool,
        include_system_files: bool,
    ) {
        let _ = self.cmd_tx.send(UsnCommand::PollChanges {
            drive_letter,
            include_hidden_files,
            include_system_files,
        });
    }

    #[allow(dead_code)]
    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(UsnCommand::Shutdown);
    }

    pub fn resp_rx_clone(&self) -> Receiver<UsnResponse> {
        self.resp_rx.clone()
    }

    /// Non-blocking check for responses
    #[allow(dead_code)]
    pub fn try_recv(&self) -> Option<UsnResponse> {
        self.resp_rx.try_recv().ok()
    }
}
