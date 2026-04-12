pub mod database;
pub mod monitor;

use crate::error::Result;
use crate::fs::get_ntfs_volumes;
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct IndexManager {
    database: Arc<Mutex<database::IndexDatabase>>,
    volume_manager: Arc<Mutex<monitor::VolumeManager>>,
}

impl IndexManager {
    pub fn new(db_path: &std::path::Path) -> Result<Self> {
        let db = database::IndexDatabase::new(db_path)?;
        let volume_manager = Arc::new(Mutex::new(monitor::VolumeManager::new()));

        Ok(Self {
            database: Arc::new(Mutex::new(db)),
            volume_manager,
        })
    }

    pub async fn search(&self, query: &str, limit: usize, offset: usize) -> Result<Vec<crate::search::SearchResult>> {
        self.database.lock().await.search(query, limit, offset).await
    }

    pub async fn get_all_files(&self) -> Result<Vec<crate::search::SearchResult>> {
        self.database.lock().await.get_all_files().await
    }

    pub async fn initialize(&self, is_admin: bool) -> Result<()> {
        log::info!("Initializing index manager...");

        let volumes = get_ntfs_volumes()?;
        
        let mut vm = self.volume_manager.lock().await;
        
        for volume in &volumes {
            vm.add_volume(&volume.drive_letter, is_admin)?;
        }

        for volume in &volumes {
            if let Some(mut monitor) = vm.take_monitor(&volume.drive_letter) {
                let scan_result = monitor.scan_with_progress(None);
                if let Ok(count) = scan_result {
                    log::info!("Scanned volume {}: {} files", volume.drive_letter, count);
                }
                vm.return_monitor(&volume.drive_letter, monitor);
            }
        }

        log::info!("Index manager initialized");
        Ok(())
    }

    pub fn get_file_count(&self) -> usize {
        self.volume_manager.blocking_lock().total_file_count()
    }
}