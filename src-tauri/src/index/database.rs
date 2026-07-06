use crate::error::Result;
use crate::search::SearchResult;
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct IndexDatabase {
    conn: Arc<Mutex<Connection>>,
}

impl IndexDatabase {
    pub fn new(db_path: &Path) -> Result<Self> {
        let conn = Connection::open(db_path)?;
        
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA cache_size = -64000;
             PRAGMA temp_store = MEMORY;
             
             CREATE TABLE IF NOT EXISTS files (
                 id INTEGER PRIMARY KEY,
                 file_id INTEGER NOT NULL,
                 name TEXT NOT NULL,
                 path TEXT NOT NULL UNIQUE,
                 parent_id INTEGER DEFAULT 0,
                 size INTEGER DEFAULT 0,
                 created_time TEXT,
                 modified_time TEXT,
                 accessed_time TEXT,
                 is_directory INTEGER DEFAULT 0,
                 attributes INTEGER DEFAULT 0,
                 volume_id INTEGER
             );
             
             CREATE INDEX IF NOT EXISTS idx_name ON files(name);
             CREATE INDEX IF NOT EXISTS idx_path ON files(path);
             CREATE INDEX IF NOT EXISTS idx_size ON files(size);
             CREATE INDEX IF NOT EXISTS idx_modified ON files(modified_time);
             
             CREATE TABLE IF NOT EXISTS volumes (
                 id INTEGER PRIMARY KEY,
                 drive_letter TEXT NOT NULL UNIQUE,
                 volume_name TEXT,
                 total_size INTEGER,
                 free_space INTEGER,
                 last_scan_time TEXT
             );"
        )?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub async fn search(&self, query: &str, limit: usize, offset: usize) -> Result<Vec<SearchResult>> {
        let conn = self.conn.lock().await;

        let mut stmt = conn.prepare(
            "SELECT id, file_id, name, path, size, modified_time, is_directory
             FROM files
             WHERE name LIKE ?1 OR path LIKE ?1
             ORDER BY name
             LIMIT ?2 OFFSET ?3"
        )?;

        let search_pattern = format!("%{}%", query);

        let rows = stmt.query_map(params![search_pattern, limit as i64, offset as i64], |row| {
            let name: String = row.get(2)?;
            let path: String = row.get(3)?;
            let modified_str: Option<String> = row.get(5)?;
            let modified_ts = modified_str.as_ref()
                .and_then(|s| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").ok())
                .map(|dt| dt.and_utc().timestamp())
                .unwrap_or(0);

            Ok(SearchResult {
                file_id: row.get(1)?,
                name: name.into(),
                path: path.into(),
                size: row.get(4)?,
                modified_time: modified_ts,
                is_directory: row.get::<_, i32>(6)? != 0,
            })
        })?;

        let mut results = Vec::new();
        for row in rows {
            if let Ok(result) = row {
                results.push(result);
            }
        }

        Ok(results)
    }

    #[allow(dead_code)]
    pub async fn get_all_files(&self) -> Result<Vec<SearchResult>> {
        let conn = self.conn.lock().await;

        let mut stmt = conn.prepare(
            "SELECT id, file_id, name, path, size, modified_time, is_directory
             FROM files
             ORDER BY name
             LIMIT 10000"
        )?;

        let rows = stmt.query_map([], |row| {
            let name: String = row.get(2)?;
            let path: String = row.get(3)?;
            let modified_str: Option<String> = row.get(5)?;
            let modified_ts = modified_str.as_ref()
                .and_then(|s| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").ok())
                .map(|dt| dt.and_utc().timestamp())
                .unwrap_or(0);

            Ok(SearchResult {
                file_id: row.get(1)?,
                name: name.into(),
                path: path.into(),
                size: row.get(4)?,
                modified_time: modified_ts,
                is_directory: row.get::<_, i32>(6)? != 0,
            })
        })?;

        let mut results = Vec::new();
        for row in rows {
            if let Ok(result) = row {
                results.push(result);
            }
        }

        Ok(results)
    }

    #[allow(dead_code)]
    pub async fn upsert_file(&self, file: &SearchResult, volume_id: i64) -> Result<()> {
        let conn = self.conn.lock().await;

        let modified_str = chrono::DateTime::from_timestamp(file.modified_time, 0)
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_default();

        conn.execute(
            "INSERT OR REPLACE INTO files
             (file_id, name, path, size, modified_time, is_directory, volume_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                file.file_id as i64,
                file.name.as_ref(),
                file.path.as_ref(),
                file.size as i64,
                modified_str,
                file.is_directory as i32,
                volume_id
            ],
        )?;

        Ok(())
    }

    #[allow(dead_code)]
    pub async fn optimize(&self) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute_batch("VACUUM; ANALYZE;")?;
        Ok(())
    }
}