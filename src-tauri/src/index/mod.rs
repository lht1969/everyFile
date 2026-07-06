pub mod database;
pub mod monitor;

use crate::error::Result;
use crate::fs::get_ntfs_volumes;
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct IndexManager {
    database: Arc<Mutex<database::IndexDatabase>>,
    #[allow(dead_code)]
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

    #[allow(dead_code)]
    pub async fn initialize(&self, is_admin: bool) -> Result<()> {
        log::info!("Initializing index manager...");

        let volumes = get_ntfs_volumes()?;
        
        // 加载配置，获取索引设置
        let config = crate::config::Config::load().ok();
        let include_hidden_files = config.as_ref().map(|c| c.index_settings.include_hidden_files).unwrap_or(false);
        let include_system_files = config.as_ref().map(|c| c.index_settings.include_system_files).unwrap_or(false);
        
        let mut vm = self.volume_manager.lock().await;
        
        for volume in &volumes {
            vm.add_volume(&volume.drive_letter, is_admin, include_hidden_files, include_system_files)?;
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

    #[allow(dead_code)]
    pub fn get_file_count(&self) -> usize {
        self.volume_manager.blocking_lock().total_file_count()
    }
}