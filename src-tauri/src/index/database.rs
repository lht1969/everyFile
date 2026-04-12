use crate::error::{AppError, Result};
use crate::search::{SearchOptions, SearchResult, SortBy, SortDirection};
use chrono::{DateTime, Local};
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
            "SELECT id, file_id, name, path, parent_id, size, created_time, modified_time, 
                    accessed_time, is_directory, attributes 
             FROM files 
             WHERE name LIKE ?1 OR path LIKE ?1
             ORDER BY name 
             LIMIT ?2 OFFSET ?3"
        )?;

        let search_pattern = format!("%{}%", query);
        
        let rows = stmt.query_map(params![search_pattern, limit as i64, offset as i64], |row| {
            let created_str: Option<String> = row.get(6)?;
            let modified_str: Option<String> = row.get(7)?;
            let accessed_str: Option<String> = row.get(8)?;

            let created_time = created_str.as_ref().and_then(|s| DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").ok()).map(|dt| dt.with_timezone(&Local)).unwrap_or_else(Local::now);
            let modified_time = modified_str.as_ref().and_then(|s| DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").ok()).map(|dt| dt.with_timezone(&Local)).unwrap_or_else(Local::now);
            let accessed_time = accessed_str.as_ref().and_then(|s| DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").ok()).map(|dt| dt.with_timezone(&Local)).unwrap_or_else(Local::now);

            Ok(SearchResult {
                file_id: row.get(1)?,
                name: row.get(2)?,
                path: row.get(3)?,
                parent_id: row.get(4)?,
                size: row.get(5)?,
                created_time,
                modified_time,
                accessed_time,
                is_directory: row.get::<_, i32>(9)? != 0,
                attributes: row.get(10)?,
                score: 1.0,
                formatted_size: SearchResult::format_size_static(row.get(5)?),
                formatted_created_time: created_str.unwrap_or_default(),
                formatted_modified_time: modified_str.unwrap_or_default(),
                formatted_accessed_time: accessed_str.unwrap_or_default(),
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

    pub async fn get_all_files(&self) -> Result<Vec<SearchResult>> {
        let conn = self.conn.lock().await;
        
        let mut stmt = conn.prepare(
            "SELECT id, file_id, name, path, parent_id, size, created_time, modified_time, 
                    accessed_time, is_directory, attributes 
             FROM files 
             ORDER BY name 
             LIMIT 10000"
        )?;

        let rows = stmt.query_map([], |row| {
            let created_str: Option<String> = row.get(6)?;
            let modified_str: Option<String> = row.get(7)?;
            let accessed_str: Option<String> = row.get(8)?;

            let created_time = created_str.as_ref().and_then(|s| DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").ok()).map(|dt| dt.with_timezone(&Local)).unwrap_or_else(Local::now);
            let modified_time = modified_str.as_ref().and_then(|s| DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").ok()).map(|dt| dt.with_timezone(&Local)).unwrap_or_else(Local::now);
            let accessed_time = accessed_str.as_ref().and_then(|s| DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").ok()).map(|dt| dt.with_timezone(&Local)).unwrap_or_else(Local::now);

            Ok(SearchResult {
                file_id: row.get(1)?,
                name: row.get(2)?,
                path: row.get(3)?,
                parent_id: row.get(4)?,
                size: row.get(5)?,
                created_time,
                modified_time,
                accessed_time,
                is_directory: row.get::<_, i32>(9)? != 0,
                attributes: row.get(10)?,
                score: 1.0,
                formatted_size: SearchResult::format_size_static(row.get(5)?),
                formatted_created_time: created_str.unwrap_or_default(),
                formatted_modified_time: modified_str.unwrap_or_default(),
                formatted_accessed_time: accessed_str.unwrap_or_default(),
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

    pub async fn upsert_file(&self, file: &SearchResult, volume_id: i64) -> Result<()> {
        let conn = self.conn.lock().await;
        
        conn.execute(
            "INSERT OR REPLACE INTO files 
             (file_id, name, path, parent_id, size, created_time, modified_time, accessed_time, is_directory, attributes, volume_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                file.file_id as i64,
                file.name,
                file.path,
                file.parent_id as i64,
                file.size as i64,
                file.created_time.format("%Y-%m-%d %H:%M:%S").to_string(),
                file.modified_time.format("%Y-%m-%d %H:%M:%S").to_string(),
                file.accessed_time.format("%Y-%m-%d %H:%M:%S").to_string(),
                file.is_directory as i32,
                file.attributes as i32,
                volume_id
            ],
        )?;

        Ok(())
    }

    pub async fn optimize(&self) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute_batch("VACUUM; ANALYZE;")?;
        Ok(())
    }
}