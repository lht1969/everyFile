//! 文件日志模块
//!
//! 将日志同时输出到控制台和文件。
//! 日志文件位置: `%APPDATA%\Everything\logs\everything-YYYY-MM-DD.log`
//!
//! 这样即使程序以静默模式（开机自启动）启动，
//! 用户也能在日志文件中查看程序是否真的启动以及启动过程。

use chrono::Local;
use log::{LevelFilter, Metadata, Record};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// 日志文件目录名
const LOG_DIR_NAME: &str = "Everything";
const LOG_SUBDIR_NAME: &str = "logs";

/// 单个日志文件最大大小（字节）：5MB
const MAX_LOG_FILE_SIZE: u64 = 5 * 1024 * 1024;

/// 保留的日志文件最大数量
const MAX_LOG_FILES: usize = 7;

/// 全局文件句柄，使用 Mutex 保证线程安全
static LOG_FILE: Mutex<Option<File>> = Mutex::new(None);

/// 获取日志目录完整路径: `%APPDATA%\Everything\logs\`
///
/// # 返回值
/// 成功时返回日志目录路径，失败时返回 None
fn get_log_dir() -> Option<PathBuf> {
    // 优先使用 dirs::data_dir() 获取 APPDATA 目录
    let base = dirs::data_dir().or_else(|| {
        // 备选：使用 USERPROFILE 环境变量
        std::env::var("USERPROFILE")
            .ok()
            .map(|p| PathBuf::from(p).join("AppData").join("Roaming"))
    })?;

    let log_dir = base.join(LOG_DIR_NAME).join(LOG_SUBDIR_NAME);
    // 创建目录（如果失败则返回 None）
    fs::create_dir_all(&log_dir).ok()?;
    Some(log_dir)
}

/// 获取当前日志文件路径
///
/// 文件名格式: `everything-YYYY-MM-DD.log`
fn get_log_file_path() -> Option<PathBuf> {
    let dir = get_log_dir()?;
    let date = Local::now().format("%Y-%m-%d").to_string();
    let filename = format!("everything-{}.log", date);
    Some(dir.join(filename))
}

/// 检查并执行日志滚动
///
/// 当当前日志文件超过 `MAX_LOG_FILE_SIZE` 时：
/// 1. 关闭当前文件
/// 2. 将其重命名为带序号的备份
/// 3. 删除超过数量限制的旧文件
/// 4. 重新打开新的日志文件
fn rotate_if_needed() {
    // 先取出当前文件并临时关闭（释放锁）
    let mut guard = match LOG_FILE.lock() {
        Ok(g) => g,
        Err(_) => return,
    };

    let path = match get_log_file_path() {
        Some(p) => p,
        None => return,
    };

    // 检查文件大小
    let needs_rotate = path
        .metadata()
        .map(|m| m.len() >= MAX_LOG_FILE_SIZE)
        .unwrap_or(false);

    if !needs_rotate {
        return;
    }

    // 需要滚动：关闭当前文件
    *guard = None;
    drop(guard);

    // 滚动现有文件: everything-YYYY-MM-DD.log -> everything-YYYY-MM-DD.1.log
    let timestamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
    let backup_path = path.with_extension(format!("{}.log.bak", timestamp));
    let _ = fs::rename(&path, &backup_path);

    // 清理过期的日志文件
    if let Some(dir) = get_log_dir() {
        cleanup_old_logs(&dir);
    }

    // 重新打开新文件
    if let Ok(mut g) = LOG_FILE.lock() {
        *g = open_log_file();
    }
}

/// 删除超过数量限制的旧日志文件
fn cleanup_old_logs(dir: &PathBuf) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut logs: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("everything-") && n.ends_with(".log"))
                    .unwrap_or(false)
        })
        .collect();

    // 按修改时间降序排序（最新的在前）
    logs.sort_by(|a, b| {
        let ma = a.metadata().and_then(|m| m.modified()).ok();
        let mb = b.metadata().and_then(|m| m.modified()).ok();
        mb.cmp(&ma)
    });

    // 保留前 MAX_LOG_FILES 个，删除其余
    for old in logs.iter().skip(MAX_LOG_FILES) {
        let _ = fs::remove_file(old);
    }
}

/// 打开日志文件（追加模式）
fn open_log_file() -> Option<File> {
    let path = get_log_file_path()?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()
}

/// 初始化文件日志
///
/// 应在程序启动早期调用，确保后续日志能被记录。
/// 调用后会自动打开当前日期的日志文件。
pub fn init() {
    if let Ok(mut guard) = LOG_FILE.lock() {
        *guard = open_log_file();
        if guard.is_some() {
            eprintln!(
                "[file_logger] 日志文件初始化成功: {:?}",
                get_log_file_path()
            );
        }
    }
}

/// 写入一行日志到文件
///
/// 在写入前会自动检查并执行日志滚动。
fn write_to_file(line: &str) {
    rotate_if_needed();

    if let Ok(mut guard) = LOG_FILE.lock() {
        if let Some(file) = guard.as_mut() {
            let _ = writeln!(file, "{}", line);
            let _ = file.flush();
        }
    }
}

/// 自定义日志实现，同时输出到 stderr 和文件
pub struct DualLogger;

impl log::Log for DualLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= LevelFilter::Info
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        if record.target().contains("usn_worker") {
            return;
        }

        // 格式: 2026-07-06 14:30:25.123 INFO  [main] message
        let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
        let level = record.level();
        let target = record.target();
        let args = record.args();

        let line = format!("{} {:<5} [{}] {}", timestamp, level, target, args);

        // 输出到 stderr（控制台）
        eprintln!("{}", line);

        // 输出到文件
        write_to_file(&line);
    }

    fn flush(&self) {
        if let Ok(mut guard) = LOG_FILE.lock() {
            if let Some(file) = guard.as_mut() {
                let _ = file.flush();
            }
        }
    }
}

/// 获取日志目录路径（公开，供调试或显示给用户）
pub fn log_dir_path() -> Option<PathBuf> {
    get_log_dir()
}
