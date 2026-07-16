pub mod aligned_reader;
pub mod error;
pub mod path;
pub mod scanner;

use std::fs::File;
use std::panic::{AssertUnwindSafe, catch_unwind};

pub use aligned_reader::AlignedReader;
pub use error::MftError;
pub use scanner::{MftScanner, ScanOutput, ScanStats};

#[allow(dead_code)]
pub struct NtfsInfo {
    pub cluster_size: u32,
    pub sector_size: u16,
}

#[allow(dead_code)]
pub fn scan_volume(path: &str, max_records: u64) -> Result<(ScanOutput, NtfsInfo), MftError> {
    let (mut reader, ntfs, info) = open_volume(path)?;
    let mut scanner = MftScanner::new(&ntfs, &mut reader)?;
    let output = scanner.scan(&mut reader, max_records);
    Ok((output, info))
}

/// 流式扫描卷：对每条 MFT 记录调用 callback，不收集到 Vec
///
/// 内存优势：恒定占用（~1MB record_buf），不随文件数量增长
/// 对比 scan_volume 在 221万文件下 ~260MB 的 all_records，显著降低峰值
///
/// 用法示例：
/// ```ignore
/// let stats = scan_volume_streaming(path, u64::MAX, &mut |record| {
///     // 处理每条记录
/// })?;
/// ```
pub fn scan_volume_streaming<F: FnMut(&scanner::ScanResult)>(
    path: &str,
    max_records: u64,
    callback: &mut F,
) -> Result<(ScanStats, std::collections::HashMap<u64, u64>), MftError> {
    let (mut reader, ntfs, _info) = open_volume(path)?;
    let mut scanner = MftScanner::new(&ntfs, &mut reader)?;
    let result = scanner.scan_streaming(&mut reader, max_records, callback);
    Ok(result)
}

pub fn open_volume(path: &str) -> Result<(AlignedReader, ntfs::Ntfs, NtfsInfo), MftError> {
    let file = File::open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            MftError::PermissionDenied {
                path: path.to_string(),
            }
        } else {
            MftError::Io(e)
        }
    })?;

    let mut reader = AlignedReader::new(file, 512)?;

    let mut ntfs = catch_unwind(AssertUnwindSafe(|| ntfs::Ntfs::new(&mut reader)))
        .map_err(|payload| {
            let msg = payload
                .downcast_ref::<String>()
                .map(|s| s.as_str())
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("unknown panic");
            MftError::NtfsPanic(msg.to_string())
        })
        .and_then(|result| result.map_err(|e| MftError::NtfsParse(e.to_string())))?;

    let _ = ntfs.read_upcase_table(&mut reader);

    let info = NtfsInfo {
        cluster_size: ntfs.cluster_size(),
        sector_size: ntfs.sector_size(),
    };

    Ok((reader, ntfs, info))
}
