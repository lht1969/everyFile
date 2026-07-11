pub mod database;
pub mod monitor;

use crate::error::Result;
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
}