//! File metadata extraction via parallel `GetFileAttributesExW` calls.
//!
//! 优化策略：使用 rayon 并行分块，对每批文件调用 `GetFileAttributesExW`
//! 批量获取元数据。相比逐文件 `std::fs::metadata`：
//! - 使用原生 Win32 API 减少 std 抽象开销
//! - `GetFileAttributesExW` 在 NTFS 上查询单文件元数据比 `std::fs::metadata` 略快
//!   （后者多了一次 open 句柄的开销）
//! - 并行分块（4096）能充分利用多核，避开 28 万目录遍历的开销
//!
//! 注意：早期版本的 `batch_metadata` 采用按目录 `FindFirstFileExW` 批量获取，
//! 在 28.6 万目录场景下反而比逐文件查询慢 3-5 倍（需要枚举每个目录的所有子条目），
//! 已弃用，回归到并行单文件查询的稳定高效方案。

use rayon::prelude::*;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use windows::core::HSTRING;
use windows::Win32::{
    Foundation::{FILETIME, HANDLE},
    Storage::FileSystem::{
        CreateFileW, GetFileAttributesExW, GetFileExInfoStandard, FILE_GENERIC_READ,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, WIN32_FILE_ATTRIBUTE_DATA,
    },
    System::Ioctl::{FSCTL_ENUM_USN_DATA, MFT_ENUM_DATA_V0},
    System::IO::DeviceIoControl,
};

/// Open a volume handle for USN operations.
pub(crate) fn open_volume_handle(drive_letter: char) -> Option<HANDLE> {
    let volume_path = HSTRING::from(format!("\\\\.\\{}:", drive_letter));
    let handle = unsafe {
        CreateFileW(
            &volume_path,
            FILE_GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    };
    handle.ok()
}

/// Metadata extracted for a single file.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FileMetadata {
    pub size: u64,
    pub modified_time: i64,
    pub is_directory: bool,
}

/// Simple wrapper for metadata lookups.
pub(crate) struct UsnMetadataReader {
    _marker: (),
}

impl UsnMetadataReader {
    pub fn new(_handle: HANDLE) -> Self {
        Self { _marker: () }
    }

    /// Get file metadata using only std::fs::metadata.
    /// Eliminates the per-file FSCTL_READ_FILE_USN_DATA kernel call.
    ///
    /// `fid` parameter is kept for API compatibility but not used.
    #[inline]
    pub fn get_file_metadata(&mut self, _fid: u64, path: &std::path::Path) -> FileMetadata {
        match std::fs::metadata(path) {
            Ok(m) => {
                let modified_time = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                FileMetadata {
                    size: m.len(),
                    modified_time,
                    is_directory: m.is_dir(),
                }
            }
            Err(_) => FileMetadata {
                size: 0,
                modified_time: 0,
                is_directory: false,
            },
        }
    }
}

/// FILETIME 转换为 Unix 时间戳（秒）
///
/// FILETIME 是自 1601-01-01 UTC 起的 100 纳秒间隔数
/// Unix 时间戳是自 1970-01-01 UTC 起的秒数
/// 两者差值：11644473600 秒
#[inline]
fn filetime_to_unix(ft: &FILETIME) -> i64 {
    let filetime = ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64);
    ((filetime / 10_000_000) as i64) - 11_644_473_600
}

/// 批量并行获取文件元数据
///
/// 使用 `rayon::par_chunks` 并行分块，每块内调用 `GetFileAttributesExW`
/// 批量查询文件大小和修改时间。
///
/// 设计要点：
/// - 相比 `std::fs::metadata`，`GetFileAttributesExW` 少一次 open 句柄的系统调用
/// - 并行分块（4096）能充分利用多核 CPU 和 NTFS 元数据缓存
/// - 不遍历目录，避免 28 万目录场景下 `FindFirstFileExW` 枚举整个目录的开销
///
/// # 参数
/// - `entries`: (路径, 是否目录) 的列表
///
/// # 返回
/// - `Vec<(u64, i64)>`: 每个条目对应的 (文件大小, 修改时间戳)
pub fn batch_metadata(entries: &[(compact_str::CompactString, bool)]) -> Vec<(u64, i64)> {
    const CHUNK_SIZE: usize = 4096;

    // 并行分块处理：每块 4096 个文件
    // 在 32 核机器上约 540 个块，恰好填满 rayon 默认线程池
    let results: Vec<(usize, u64, i64)> = entries
        .par_chunks(CHUNK_SIZE)
        .enumerate()
        .flat_map(|(chunk_idx, chunk)| {
            let mut results = Vec::with_capacity(chunk.len());
            for (i, (path_str, is_dir)) in chunk.iter().enumerate() {
                let global_idx = chunk_idx * CHUNK_SIZE + i;
                let (size, modified_time) = query_one(path_str);
                // 目录的大小统一设为 0（业务约定）
                let final_size = if *is_dir { 0 } else { size };
                results.push((global_idx, final_size, modified_time));
            }
            results
        })
        .collect();

    // 按原始索引构建有序结果数组
    let mut meta_by_index = vec![(0u64, 0i64); entries.len()];
    for (idx, size, modified_time) in results {
        if idx < meta_by_index.len() {
            meta_by_index[idx] = (size, modified_time);
        }
    }

    meta_by_index
}

/// 单文件元数据查询（线程安全，可在 rayon 并行上下文调用）
///
/// `GetFileAttributesExW` 在 NTFS 文件系统上是查询单文件元数据最快的 API，
/// 比 `std::fs::metadata` 少一次 open 句柄的系统调用。
///
/// 暴露为 `pub` 以便在路径解析阶段就地合并调用，避免二次遍历。
#[inline]
pub fn query_one(path_str: &str) -> (u64, i64) {
    let path_h = HSTRING::from(path_str);
    let mut file_info = WIN32_FILE_ATTRIBUTE_DATA::default();
    let ok = unsafe {
        GetFileAttributesExW(
            &path_h,
            GetFileExInfoStandard,
            &mut file_info as *mut _ as *mut std::ffi::c_void,
        )
    };
    if ok.is_ok() {
        let size = ((file_info.nFileSizeHigh as u64) << 32) | (file_info.nFileSizeLow as u64);
        let modified_time = filetime_to_unix(&file_info.ftLastWriteTime);
        (size, modified_time)
    } else {
        // 文件可能在 MFT 快照后被删除，或权限不足，使用默认值
        (0, 0)
    }
}

/// 自定义 MFT 条目（扩展 usn-journal-rs 的 MftEntry，增加 timestamp 字段）
///
/// 关键差异：直接从 USN_RECORD_V2 缓冲区解析 timestamp，
/// 避免后续 Phase 2 调用 `GetFileAttributesExW` 的 IO 开销。
pub struct MftEntryWithTimestamp {
    pub fid: u64,
    pub parent_fid: u64,
    pub file_name: OsString,
    pub file_attributes: u32,
    /// USN 记录的事件时间戳（FILETIME 100ns 单位，1601 起）
    /// 对于"无后续变化"的文件，该值约等于文件最后修改时间
    pub timestamp: i64,
}

/// 将 USN_RECORD_V2 时间戳（FILETIME 100ns）转换为 Unix 秒
#[inline]
fn usn_timestamp_to_unix(usn_ts_100ns: i64) -> i64 {
    (usn_ts_100ns / 10_000_000) - 11_644_473_600
}

/// 直接调用 `FSCTL_ENUM_USN_DATA` 枚举 MFT 条目，携带 timestamp 字段
///
/// 相比 `usn-journal-rs` 的 `mft.iter_with_options`，本实现：
/// - 跳过 `MftEntry` 包装结构，直接返回含 timestamp 的轻量结构
/// - 自定义缓冲区大小（默认 4MB），减少 `DeviceIoControl` 调用
/// - 内联 `MftEntry` 字段拷贝，节省一次堆分配
///
/// # 参数
/// - `handle`: 通过 `open_volume_handle` 打开的卷句柄
/// - `buffer_size`: MFT 枚举缓冲区大小（默认 4MB）
///
/// # 返回
/// - `Ok(Vec<MftEntryWithTimestamp>)`: 成功枚举的条目列表
/// - `Err(String)`: 错误信息
pub fn enumerate_mft_with_timestamps(
    handle: HANDLE,
    buffer_size: usize,
) -> Result<Vec<MftEntryWithTimestamp>, String> {
    let mut entries: Vec<MftEntryWithTimestamp> = Vec::with_capacity(1_000_000);
    let mut buffer: Vec<u8> = vec![0u8; buffer_size];
    let mut next_start_fid: u64 = 0;

    loop {
        let mft_enum_data = MFT_ENUM_DATA_V0 {
            StartFileReferenceNumber: next_start_fid,
            LowUsn: 0,
            HighUsn: i64::MAX,
        };

        let mut bytes_returned: u32 = 0;
        let result = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_ENUM_USN_DATA,
                Some(&mft_enum_data as *const _ as *const std::ffi::c_void),
                std::mem::size_of::<MFT_ENUM_DATA_V0>() as u32,
                Some(buffer.as_mut_ptr() as *mut std::ffi::c_void),
                buffer.len() as u32,
                Some(&mut bytes_returned),
                None,
            )
        };

        match result {
            Ok(()) => {
                if (bytes_returned as usize) < std::mem::size_of::<u64>() {
                    // 缓冲区首部 8 字节是 next_start_fid，少于 8 字节视为结束
                    break;
                }

                // 缓冲区首 8 字节是 next_start_fid（下次调用的 StartFileReferenceNumber）
                let next_start = u64::from_le_bytes([
                    buffer[0], buffer[1], buffer[2], buffer[3], buffer[4], buffer[5], buffer[6],
                    buffer[7],
                ]);

                // 解析 USN_RECORD_V2 记录（从 offset=8 开始）
                let mut offset = std::mem::size_of::<u64>();
                while offset < bytes_returned as usize {
                    // 至少需要 RecordLength(4) + MajorVersion(2) + MinorVersion(2) = 8 字节头
                    if offset + 8 > bytes_returned as usize {
                        break;
                    }

                    let record_length = u32::from_le_bytes([
                        buffer[offset],
                        buffer[offset + 1],
                        buffer[offset + 2],
                        buffer[offset + 3],
                    ]);
                    if record_length == 0 {
                        break;
                    }
                    if offset + record_length as usize > bytes_returned as usize {
                        break;
                    }

                    // USN_RECORD_V2 布局（与 read_usn_records_direct 中一致）：
                    // +0:  RecordLength (u32)
                    // +4:  MajorVersion (u16)
                    // +6:  MinorVersion (u16)
                    // +8:  FileReferenceNumber (u64)
                    // +16: ParentFileReferenceNumber (u64)
                    // +24: Usn (i64)
                    // +32: TimeStamp (i64) ← 关键：从 MFT 枚举直接获取
                    // +40: Reason (u32)
                    // +48: SourceInfo (u32)
                    // +52: SecurityId (u32)
                    // +56: FileAttributes (u32)
                    // +60: FileNameLength (u16, 字节数)
                    // +62: FileNameOffset (u16, 从记录起始的偏移)
                    // +64: FileName (UTF-16, 零终止)

                    let major_version =
                        u16::from_le_bytes([buffer[offset + 4], buffer[offset + 5]]);
                    if major_version != 2 {
                        // 不支持的版本，跳过该记录
                        offset += record_length as usize;
                        continue;
                    }

                    let fid = u64::from_le_bytes([
                        buffer[offset + 8],
                        buffer[offset + 9],
                        buffer[offset + 10],
                        buffer[offset + 11],
                        buffer[offset + 12],
                        buffer[offset + 13],
                        buffer[offset + 14],
                        buffer[offset + 15],
                    ]);
                    let parent_fid = u64::from_le_bytes([
                        buffer[offset + 16],
                        buffer[offset + 17],
                        buffer[offset + 18],
                        buffer[offset + 19],
                        buffer[offset + 20],
                        buffer[offset + 21],
                        buffer[offset + 22],
                        buffer[offset + 23],
                    ]);
                    let usn_ts_100ns = i64::from_le_bytes([
                        buffer[offset + 32],
                        buffer[offset + 33],
                        buffer[offset + 34],
                        buffer[offset + 35],
                        buffer[offset + 36],
                        buffer[offset + 37],
                        buffer[offset + 38],
                        buffer[offset + 39],
                    ]);
                    let file_attributes = u32::from_le_bytes([
                        buffer[offset + 52],
                        buffer[offset + 53],
                        buffer[offset + 54],
                        buffer[offset + 55],
                    ]);
                    let file_name_length =
                        u16::from_le_bytes([buffer[offset + 56], buffer[offset + 57]]) as usize;
                    let file_name_offset =
                        u16::from_le_bytes([buffer[offset + 58], buffer[offset + 59]]) as usize;

                    // 读取文件名（UTF-16 编码）
                    let name_start = offset + file_name_offset;
                    let name_end = name_start + file_name_length;
                    if name_end <= bytes_returned as usize && file_name_length.is_multiple_of(2) {
                        let name_units: Vec<u16> = buffer[name_start..name_end]
                            .chunks_exact(2)
                            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                            .collect();
                        let file_name = OsString::from_wide(&name_units);

                        entries.push(MftEntryWithTimestamp {
                            fid,
                            parent_fid,
                            file_name,
                            file_attributes,
                            timestamp: usn_timestamp_to_unix(usn_ts_100ns),
                        });
                    }

                    offset += record_length as usize;
                }

                // 更新下次调用的起始 FID
                let prev_start = next_start_fid;
                next_start_fid = next_start;

                // 如果 next_start 没变，说明没有更多条目（usn-journal-rs 用此条件退出）
                if next_start_fid == prev_start {
                    break;
                }
                // 如果 next_start 达到或超过 0xFFFFFFFF00000000（高 2 字节为文件号，低 6 字节为顺序号），
                // 说明已读完所有 MFT 条目
                if next_start >> 48 > 0 {
                    break;
                }
            }
            Err(err) => {
                // ERROR_HANDLE_EOF (38) 表示无更多数据，正常结束
                // 注意：win32 错误码在 windows-rs 中以 HRESULT 形式返回，
                // HRESULT_FROM_WIN32(38) = 0x80070026（最高位表示失败设施）
                let code = err.code().0 as u32;
                if code == 38u32 || code == 0x80070026u32 {
                    break;
                }
                return Err(format!(
                    "FSCTL_ENUM_USN_DATA failed: {} (code=0x{:x})",
                    err, code
                ));
            }
        }
    }

    Ok(entries)
}
