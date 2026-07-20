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
             PRAGMA cache_size = -4000;
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
             );

             -- USN Journal 增量扫描状态：
             --   drive_letter : 卷盘符（如 C: / D:）
             --   last_usn     : 上次成功读取到的 USN 位置（用于下次增量起点）
             --   journal_id   : 同步的 journal 标识（journal 重置时检测）
             --   updated_at   : 最后更新时间（UTC）
             -- 如果 last_usn=0 表示尚未进行过增量扫描，下次会从 journal 起点开始读
             CREATE TABLE IF NOT EXISTS usn_state (
                 drive_letter TEXT PRIMARY KEY,
                 last_usn INTEGER NOT NULL DEFAULT 0,
                 journal_id INTEGER NOT NULL DEFAULT 0,
                 updated_at TEXT
             );",
        )?;

        // 兼容旧数据库：添加 file_ref 列
        let _ = conn.execute(
            "ALTER TABLE files ADD COLUMN file_ref INTEGER DEFAULT 0",
            [],
        );

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub async fn search(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<SearchResult>> {
        let conn = self.conn.lock().await;

        let mut stmt = conn.prepare(
            "SELECT id, file_id, name, path, size, modified_time, is_directory, file_ref
             FROM files
             WHERE name LIKE ?1 OR path LIKE ?1
             ORDER BY name
             LIMIT ?2 OFFSET ?3",
        )?;

        let search_pattern = format!("%{}%", query);

        let rows = stmt.query_map(
            params![search_pattern, limit as i64, offset as i64],
            |row| {
                let name: String = row.get(2)?;
                let path: String = row.get(3)?;
                let modified_str: Option<String> = row.get(5)?;
                let modified_ts = modified_str
                    .as_ref()
                    .and_then(|s| {
                        chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").ok()
                    })
                    .map(|dt| dt.and_utc().timestamp())
                    .unwrap_or(0);

                Ok(SearchResult {
                    file_id: row.get::<_, u32>(1)?,
                    name: name.into(),
                    path: path.into(),
                    size: row.get(4)?,
                    modified_time: modified_ts,
                    is_directory: row.get::<_, i32>(6)? != 0,
                })
            },
        )?;

        let mut results = Vec::new();
        for result in rows.flatten() {
            results.push(result);
        }

        Ok(results)
    }

    /// 读取指定卷的 USN 增量状态（last_usn, journal_id）
    /// - 首次扫描时（数据库无记录）返回 (0, 0)
    /// - journal_id 用于检测 journal 是否被重置（如 fsutil usn deletejournal），
    ///   重置时调用方应忽略持久化的 last_usn，重新从 0 开始读
    #[allow(dead_code)]
    pub async fn load_usn_state(&self, drive_letter: &str) -> Result<(i64, u64)> {
        let conn = self.conn.lock().await;
        let mut stmt =
            conn.prepare("SELECT last_usn, journal_id FROM usn_state WHERE drive_letter = ?1")?;
        let mut rows = stmt.query(params![drive_letter])?;
        if let Some(row) = rows.next()? {
            let last_usn: i64 = row.get(0)?;
            let journal_id: i64 = row.get(1)?;
            Ok((last_usn, journal_id.max(0) as u64))
        } else {
            // 首次扫描：返回 (0, 0) 表示从 journal 起点开始
            Ok((0, 0))
        }
    }

    /// 持久化 USN 增量状态：扫描成功后调用，写入 last_usn 与 journal_id
    /// - UPSERT 语义：已存在则更新，不存在则插入
    /// - updated_at 写入当前 UTC 时间（YYYY-MM-DD HH:MM:SS 格式）
    #[allow(dead_code)]
    pub async fn save_usn_state(
        &self,
        drive_letter: &str,
        last_usn: i64,
        journal_id: u64,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        // 写入 UTC 时间戳（与 files 表的 modified_time 格式保持一致）
        let updated_at = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        conn.execute(
            "INSERT INTO usn_state (drive_letter, last_usn, journal_id, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(drive_letter) DO UPDATE SET
                last_usn = excluded.last_usn,
                journal_id = excluded.journal_id,
                updated_at = excluded.updated_at",
            params![drive_letter, last_usn, journal_id as i64, updated_at],
        )?;
        Ok(())
    }
}
