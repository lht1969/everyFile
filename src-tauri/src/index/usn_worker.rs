use crate::index::lib as mft_lib;
use crate::index::ntfs_mft;
use crate::index::usn_types::{UsnCommand, UsnResponse, UsnState, VolumeState};
use crate::search::SearchResult;
use crossbeam_channel::{Receiver, Sender};
use rayon::prelude::*;
use std::collections::HashMap;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::time::Instant;
use usn_journal_rs::mft::EnumOptions;
use usn_journal_rs::path::PathResolvableEntry;
use usn_journal_rs::volume::Volume;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V0,
};

/// Convert NTFS timestamp (100ns intervals since 1601-01-01) to Unix timestamp (seconds since 1970)
const NTFS_EPOCH_DIFF: i64 = 11_644_473_600; // seconds between 1601 and 1970

#[inline]
fn ntfs_time_to_unix(ntfs_time: Option<u64>) -> i64 {
    match ntfs_time {
        Some(t) if t > 0 => (t as i64 / 10_000_000) - NTFS_EPOCH_DIFF,
        _ => 0,
    }
}

/// USN_REASON_MASK_ALL：监控所有变更原因
const USN_REASON_MASK_ALL: u32 = 0xFFFFFFFF;

/// 从 FSCTL_READ_USN_JOURNAL 直接解析的原始 USN 记录
struct RawUsnRecord {
    usn: i64,
    fid: u64,
    parent_fid: u64,
    reason: u32,
    file_name: OsString,
    file_attributes: u32,
}

/// 实现 PathResolvableEntry，使 RawUsnRecord 可用于 usn-journal-rs 的路径解析
impl usn_journal_rs::path::PathResolvableEntry for RawUsnRecord {
    fn fid(&self) -> u64 { self.fid }
    fn parent_fid(&self) -> u64 { self.parent_fid }
    fn file_name(&self) -> &OsString { &self.file_name }
    fn is_dir(&self) -> bool {
        // FILE_ATTRIBUTE_DIRECTORY = 0x10
        (self.file_attributes & 0x10) != 0
    }
}

impl RawUsnRecord {
    /// 判断是否为隐藏文件（FILE_ATTRIBUTE_HIDDEN = 0x02）
    fn is_hidden(&self) -> bool {
        (self.file_attributes & 0x02) != 0
    }
}

/// 直接调用 FSCTL_READ_USN_JOURNAL 读取 USN 变更记录
///
/// 绕过 usn-journal-rs 的 iter_with_options，原因：
/// 1. iter_with_options 内部用 entry.usn 作为 last_usn，但正确的 last_usn 应为 API 返回的 next-start USN
/// 2. last_usn + 1 不是有效的记录边界，导致 ERROR_INVALID_PARAMETER (0x80070057)
/// 3. next-start USN（buffer 前 8 字节）是 UsnJournalIter 的私有字段，外部无法访问
///
/// 返回 (记录列表, next_start_usn)，next_start_usn 可作为下次调用的 start_usn
fn read_usn_records_direct(
    handle: HANDLE,
    journal_id: u64,
    start_usn: i64,
) -> Result<(Vec<RawUsnRecord>, i64), String> {
    let mut records: Vec<RawUsnRecord> = Vec::new();
    let mut current_start_usn = start_usn;
    // 64KB 缓冲区，单次可读取数百条记录
    let buffer_size: usize = 64 * 1024;
    let mut buffer = vec![0u8; buffer_size];

    loop {
        // 构造 READ_USN_JOURNAL_DATA_V0 输入参数
        let read_data = READ_USN_JOURNAL_DATA_V0 {
            StartUsn: current_start_usn,
            ReasonMask: USN_REASON_MASK_ALL,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            // BytesToWaitFor=0：非阻塞模式，有数据立即返回，无数据返回 ERROR_HANDLE_EOF
            BytesToWaitFor: 0,
            UsnJournalID: journal_id,
        };

        let mut bytes_returned: u32 = 0;
        let result = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_READ_USN_JOURNAL,
                Some(&read_data as *const _ as *const _),
                std::mem::size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
                Some(buffer.as_mut_ptr() as *mut std::ffi::c_void),
                buffer.len() as u32,
                Some(&mut bytes_returned),
                None,
            )
        };

        match result {
            Ok(()) => {
                // buffer 前 8 字节是 next_start_usn（下一次读取应使用的 StartUsn）
                if (bytes_returned as usize) < std::mem::size_of::<i64>() {
                    // 返回数据不足以包含 next_start_usn，视为无更多数据
                    break;
                }

                let next_start_usn = i64::from_le_bytes([
                    buffer[0], buffer[1], buffer[2], buffer[3],
                    buffer[4], buffer[5], buffer[6], buffer[7],
                ]);

                // 解析 USN_RECORD_V2 记录（从 offset=8 开始，跳过 next_start_usn 头部）
                let mut offset = std::mem::size_of::<i64>();
                while offset < bytes_returned as usize {
                    // 读取 RecordLength（前 4 字节）
                    if offset + 4 > bytes_returned as usize {
                        break;
                    }
                    let record_length = u32::from_le_bytes([
                        buffer[offset], buffer[offset + 1], buffer[offset + 2], buffer[offset + 3],
                    ]);

                    if record_length == 0 || offset + record_length as usize > bytes_returned as usize {
                        break;
                    }

                    // USN_RECORD_V2 布局：
                    // +0:  RecordLength (u32)
                    // +4:  MajorVersion (u16)
                    // +6:  MinorVersion (u16)
                    // +8:  FileReferenceNumber (u64)
                    // +16: ParentFileReferenceNumber (u64)
                    // +24: Usn (i64)
                    // +32: TimeStamp (i64)
                    // +40: Reason (u32)
                    // +44: SourceInfo (u32)
                    // +48: SecurityId (u32)
                    // +52: FileAttributes (u32)
                    // +56: FileNameLength (u16, 字节数)
                    // +58: FileNameOffset (u16, 从记录起始的偏移)
                    // +60: FileName (UTF-16, 零终止)
                    let fid = u64::from_le_bytes([
                        buffer[offset + 8], buffer[offset + 9], buffer[offset + 10], buffer[offset + 11],
                        buffer[offset + 12], buffer[offset + 13], buffer[offset + 14], buffer[offset + 15],
                    ]);
                    let parent_fid = u64::from_le_bytes([
                        buffer[offset + 16], buffer[offset + 17], buffer[offset + 18], buffer[offset + 19],
                        buffer[offset + 20], buffer[offset + 21], buffer[offset + 22], buffer[offset + 23],
                    ]);
                    let usn = i64::from_le_bytes([
                        buffer[offset + 24], buffer[offset + 25], buffer[offset + 26], buffer[offset + 27],
                        buffer[offset + 28], buffer[offset + 29], buffer[offset + 30], buffer[offset + 31],
                    ]);
                    let reason = u32::from_le_bytes([
                        buffer[offset + 40], buffer[offset + 41], buffer[offset + 42], buffer[offset + 43],
                    ]);
                    let file_attributes = u32::from_le_bytes([
                        buffer[offset + 52], buffer[offset + 53], buffer[offset + 54], buffer[offset + 55],
                    ]);
                    let file_name_length = u16::from_le_bytes([
                        buffer[offset + 56], buffer[offset + 57],
                    ]) as usize;
                    let file_name_offset = u16::from_le_bytes([
                        buffer[offset + 58], buffer[offset + 59],
                    ]) as usize;

                    // 读取文件名（UTF-16 编码）
                    let name_start = offset + file_name_offset;
                    let name_end = name_start + file_name_length;
                    if name_end <= bytes_returned as usize && file_name_length % 2 == 0 {
                        let name_units: Vec<u16> = buffer[name_start..name_end]
                            .chunks_exact(2)
                            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                            .collect();
                        let file_name = OsString::from_wide(&name_units);

                        records.push(RawUsnRecord {
                            usn,
                            fid,
                            parent_fid,
                            reason,
                            file_name,
                            file_attributes,
                        });
                    }

                    offset += record_length as usize;
                }

                // 更新 current_start_usn 为 API 返回的 next_start_usn
                current_start_usn = next_start_usn;

                // 如果返回的数据量远小于缓冲区，说明已读完当前所有记录
                if (bytes_returned as usize) < buffer_size / 2 {
                    break;
                }
            }
            Err(err) => {
                // ERROR_HANDLE_EOF (38) 表示无更多数据，正常结束
                let code = err.code().0 & 0x0000FFFF;
                if code == 38 {
                    break;
                }
                // 其他错误返回错误信息
                return Err(format!(
                    "FSCTL_READ_USN_JOURNAL failed: {} (code=0x{:x})",
                    err, code
                ));
            }
        }
    }

    Ok((records, current_start_usn))
}

/// Fallback path resolution using batch parent map.
/// When usn-journal-rs's resolver can't find a parent (e.g. newly created files
/// whose parent wasn't in the resolver's MFT cache), we walk the parent chain
/// using the USN records from the current batch.
fn resolve_path_from_batch(
    fid: u64,
    parent_map: &std::collections::HashMap<u64, u64>,
    name_map: &std::collections::HashMap<u64, &std::ffi::OsString>,
    drive_letter: char,
) -> Option<std::path::PathBuf> {
    let mut components: Vec<&std::ffi::OsString> = Vec::with_capacity(8);
    let mut cur = fid;

    // Walk up the parent chain (max 50 levels to prevent infinite loops)
    for _ in 0..50 {
        if let Some(name) = name_map.get(&cur) {
            let name_str = name.to_string_lossy();
            // Skip NTFS virtual entries
            if name_str != "." && name_str != ".." && !name_str.starts_with('$') {
                components.push(name);
            }
        }

        match parent_map.get(&cur) {
            Some(&pfid) if pfid != cur && pfid != 0 => {
                cur = pfid;
            }
            _ => break,
        }
    }

    if components.is_empty() {
        return None;
    }

    components.reverse();

    let mut path = std::path::PathBuf::new();
    path.push(format!("{}:", drive_letter));
    path.push("\\");
    for comp in &components {
        path.push(comp);
    }

    Some(path)
}

pub fn spawn_usn_worker(
    cmd_rx: Receiver<UsnCommand>,
    resp_tx: Sender<UsnResponse>,
) {
    std::thread::Builder::new()
        .name("usn-worker".into())
        .spawn(move || {
            worker_loop(cmd_rx, resp_tx);
        })
        .expect("failed to spawn USN worker thread");
}

fn worker_loop(cmd_rx: Receiver<UsnCommand>, resp_tx: Sender<UsnResponse>) {
    let mut volumes: HashMap<char, Volume> = HashMap::new();
    let mut last_usn_map: HashMap<char, i64> = HashMap::new();
    let mut journal_id_map: HashMap<char, u64> = HashMap::new();

    loop {
        log::info!("[USN] Worker loop waiting for command, volumes stored: {:?}", volumes.keys().collect::<Vec<_>>());
        match cmd_rx.recv() {
            Ok(UsnCommand::FullScan { drive_letter, include_hidden_files, include_system_files }) => {
                handle_full_scan(
                    drive_letter,
                    include_hidden_files,
                    include_system_files,
                    &mut volumes,
                    &mut last_usn_map,
                    &mut journal_id_map,
                    &resp_tx,
                );
            }
            Ok(UsnCommand::PollChanges { drive_letter, include_hidden_files, include_system_files }) => {
                handle_poll_changes(
                    drive_letter,
                    include_hidden_files,
                    include_system_files,
                    &volumes,
                    &mut last_usn_map,
                    &resp_tx,
                );
            }
            Ok(UsnCommand::Shutdown) | Err(_) => {
                break;
            }
        }
    }
}

fn handle_full_scan(
    drive_letter: char,
    include_hidden_files: bool,
    include_system_files: bool,
    volumes: &mut HashMap<char, Volume>,
    last_usn_map: &mut HashMap<char, i64>,
    journal_id_map: &mut HashMap<char, u64>,
    resp_tx: &Sender<UsnResponse>,
) {
    log::info!("[USN] Full scan starting for drive {}", drive_letter);
    let scan_start = Instant::now();

    // Phase 1a: Use MftScanner to read raw MFT records directly from disk
    // This bypasses FSCTL_ENUM_USN_DATA and reads data runs directly — much faster
    let t0 = Instant::now();
    let volume_path = format!("\\\\.\\{}:", drive_letter);

    let scan_output = match mft_lib::scan_volume(&volume_path, u64::MAX) {
        Ok((output, _info)) => output,
        Err(e) => {
            log::warn!("[USN] MftScanner failed for {}: {}, falling back to FSCTL_ENUM_USN_DATA", drive_letter, e);
            handle_full_scan_legacy(
                drive_letter, include_hidden_files, include_system_files,
                volumes, last_usn_map, journal_id_map, resp_tx,
            );
            return;
        }
    };

    log::info!(
        "[USN] Phase 1a: MftScanner read {} records ({} files, {} dirs) for {} in {:?}",
        scan_output.total_records, scan_output.files, scan_output.dirs,
        drive_letter, t0.elapsed()
    );

    // Phase 1b: Resolve full paths from parent_record chain
    let t1 = Instant::now();

    // Build record_number → (parent_record, name, is_dir, size, mtime, attributes) map
    // Use a Vec indexed by record_number for O(1) lookup
    let mut records_by_number: Vec<Option<(u64, &str, bool, u64, Option<u64>, u32)>> =
        vec![None; scan_output.total_records as usize + 1];
    for r in &scan_output.all_records {
        let idx = r.record_number as usize;
        if idx < records_by_number.len() {
            records_by_number[idx] = Some((
                r.parent_record,
                &r.name,
                r.is_directory,
                r.size,
                r.mtime,
                r.attributes,
            ));
        }
    }

    // Resolve paths in parallel using rayon

    const RECYCLE_BIN: &str = "$recycle.bin";
    const SYSTEM_VOL_INFO: &str = "system volume information";

    let entries: Vec<(u64, Box<str>, Box<str>, bool, i64, u64)> = scan_output
        .all_records
        .par_iter()
        .filter_map(|r| {
            let record_number = r.record_number;

            // Skip root and metadata records
            if record_number < 5 {
                return None;
            }

            // Skip unnamed / placeholder records (<Record#N>, <no name>)
            if r.name.starts_with('<') {
                return None;
            }

            // Skip $-prefixed NTFS metadata files
            if r.name.starts_with('$') {
                return None;
            }

            // Skip NTFS virtual entries (. and ..)
            if r.name == "." || r.name == ".." {
                return None;
            }

            // Hidden check
            if !include_hidden_files && (r.attributes & 0x02) != 0 {
                return None;
            }
            // System check
            if !include_system_files && (r.attributes & 0x04) != 0 {
                return None;
            }

            // Resolve path via parent chain — O(depth) with O(1) Vec lookups
            let mut parts: Vec<&str> = Vec::with_capacity(16);
            parts.push(&r.name);
            let mut cur = record_number;
            for _ in 0..50 {
                let parent = records_by_number.get(cur as usize)
                    .and_then(|opt| opt.as_ref())
                    .map(|(p, _, _, _, _, _)| *p);
                match parent {
                    Some(p) if p != cur && p != 0 => {
                        cur = p;
                        if let Some(Some((_, name, _, _, _, _))) = records_by_number.get(cur as usize) {
                            // Skip NTFS virtual entries (. and ..) to avoid paths like C:\.\windows
                            if *name == "." || *name == ".." {
                                continue;
                            }
                            parts.push(name);
                        } else {
                            break;
                        }
                    }
                    _ => break,
                }
            }
            parts.reverse();

            // Skip paths containing $-prefixed components (NTFS special directories)
            if parts.iter().any(|p| p.starts_with('$')) {
                return None;
            }

            // Build path string: "C:\folder\subfolder\file.txt"
            let estimated_len: usize = parts.iter().map(|p| p.len()).sum::<usize>()
                + parts.len()  // separators
                + 4;            // "X:\" prefix
            let path_str: Box<str> = {
                let mut path = String::with_capacity(estimated_len);
                path.push(drive_letter);
                path.push(':');
                path.push('\\');
                for (i, part) in parts.iter().enumerate() {
                    if i > 0 {
                        path.push('\\');
                    }
                    path.push_str(part);
                }
                path.into()
            };

            // Path component checks
            let skip = path_str.split('\\').any(|comp| {
                let sl = comp.to_lowercase();
                sl == RECYCLE_BIN || (!include_system_files && sl == SYSTEM_VOL_INFO)
            });
            if skip {
                return None;
            }

            let name_str: Box<str> = Box::from(r.name.as_str());
            let is_dir = r.is_directory;
            let size = r.size;
            let modified_time = ntfs_time_to_unix(r.mtime);
            let file_id = r.record_number;

            Some((file_id, name_str, path_str, is_dir, modified_time, size))
        })
        .collect();

    log::info!(
        "[USN] Phase 1b: Resolved {} file paths for {} in {:?}",
        entries.len(), drive_letter, t1.elapsed()
    );

    // Free records_by_number and scan_output — no longer needed
    // Drop records_by_number first since it borrows &str from scan_output
    drop(records_by_number);
    drop(scan_output);

    // Phase 2: Build SearchResult entries (size already from MFT, no need for batch_metadata)
    let t2 = Instant::now();
    let files: Vec<SearchResult> = entries
        .into_iter()
        .map(|(fid, name_str, path_str, is_dir, modified_time, size)| {
            SearchResult {
                file_id: fid,
                name: name_str,
                path: path_str,
                size,
                modified_time,
                is_directory: is_dir,
            }
        })
        .collect();

    log::info!(
        "[USN] Phase 2: Built {} SearchResult entries in {:?}",
        files.len(), t2.elapsed()
    );
    log::info!(
        "[USN] Full scan complete for drive {}: {} files, total {:?}",
        drive_letter, files.len(), scan_start.elapsed()
    );

    // Create or verify USN journal (required for incremental updates)
    let volume = match Volume::from_drive_letter(drive_letter) {
        Ok(v) => v,
        Err(e) => {
            let _ = resp_tx.send(UsnResponse::Error {
                message: format!("Failed to open volume {} for journal: {}", drive_letter, e),
            });
            return;
        }
    };

    let journal = volume.journal();
    let journal_max_size = usn_journal_rs::DEFAULT_JOURNAL_MAX_SIZE;
    let allocation_delta = usn_journal_rs::DEFAULT_JOURNAL_ALLOCATION_DELTA;

    let (journal_id, last_usn) = match journal.query(true) {
        Ok(data) => {
            if data.maximum_size < journal_max_size {
                if let Err(e) = journal.create_or_update(journal_max_size, allocation_delta) {
                    log::warn!("[USN] Failed to resize journal for {}: {}", drive_letter, e);
                }
                if let Ok(new_data) = journal.query(false) {
                    (new_data.journal_id, new_data.next_usn)
                } else {
                    (data.journal_id, data.next_usn)
                }
            } else {
                (data.journal_id, data.next_usn)
            }
        }
        Err(e) => {
            log::warn!("[USN] Failed to query journal for {}: {}", drive_letter, e);
            match journal.create_or_update(journal_max_size, allocation_delta) {
                Ok(()) => match journal.query(false) {
                    Ok(data) => (data.journal_id, data.next_usn),
                    Err(e2) => {
                        let _ = resp_tx.send(UsnResponse::Error {
                            message: format!("Journal create+query failed for {}: {}", drive_letter, e2),
                        });
                        return;
                    }
                },
                Err(e2) => {
                    let _ = resp_tx.send(UsnResponse::Error {
                        message: format!("Journal create failed for {}: {}", drive_letter, e2),
                    });
                    return;
                }
            }
        }
    };

    last_usn_map.insert(drive_letter, last_usn);
    journal_id_map.insert(drive_letter, journal_id);

    let mut state = UsnState::load();
    state.volumes.insert(
        drive_letter.to_string(),
        VolumeState { journal_id, last_usn },
    );
    state.save();

    volumes.insert(drive_letter, volume);

    let _ = resp_tx.send(UsnResponse::FullScanResult {
        drive_letter,
        files,
        last_usn,
        journal_id,
    });
}

fn handle_full_scan_legacy(
    drive_letter: char,
    include_hidden_files: bool,
    include_system_files: bool,
    volumes: &mut HashMap<char, Volume>,
    last_usn_map: &mut HashMap<char, i64>,
    journal_id_map: &mut HashMap<char, u64>,
    resp_tx: &Sender<UsnResponse>,
) {
    log::info!("[USN] Full scan (legacy) starting for drive {}", drive_letter);
    let scan_start = Instant::now();

    let volume = match Volume::from_drive_letter(drive_letter) {
        Ok(v) => v,
        Err(e) => {
            let _ = resp_tx.send(UsnResponse::Error {
                message: format!("Failed to open volume {}: {}", drive_letter, e),
            });
            return;
        }
    };

    let mft = volume.mft();

    const RECYCLE_BIN: &str = "$recycle.bin";
    const SYSTEM_VOL_INFO: &str = "system volume information";

    // Phase 1a: 自定义 MFT 枚举，直接从 USN_RECORD_V2 提取 timestamp
    // 4MB 缓冲区：减少 DeviceIoControl 调用次数至 2-3 次
    // 关键收益：timestamp 字段一次性拿到，省去后续 GetFileAttributesExW 查询
    let t0 = Instant::now();
    struct RawEntry {
        fid: u64,
        file_name: std::ffi::OsString,
        file_attributes: u32,
        timestamp: i64,
    }

    let mut raw_entries: Vec<RawEntry>;
    let mut parent_map: HashMap<u64, u64>;

    // 优先尝试自定义枚举器（携带 timestamp）
    // 失败时回退到 usn-journal-rs 的标准枚举器（无 timestamp）
    // 缓冲区选择：1MB 在 221 万文件下最优（实测 9.5s），
    // 4MB 反而因 buffer_size/2 退出条件过严导致多轮调用
    let vol_handle_for_mft = ntfs_mft::open_volume_handle(drive_letter);
    match vol_handle_for_mft {
        Some(h) => {
            match ntfs_mft::enumerate_mft_with_timestamps(h, 1024 * 1024) {
                Ok(entries_with_ts) => {
                    raw_entries = Vec::with_capacity(entries_with_ts.len());
                    parent_map = HashMap::with_capacity(entries_with_ts.len());
                    for e in entries_with_ts {
                        parent_map.insert(e.fid, e.parent_fid);
                        raw_entries.push(RawEntry {
                            fid: e.fid,
                            file_name: e.file_name,
                            file_attributes: e.file_attributes,
                            timestamp: e.timestamp,
                        });
                    }
                }
                Err(e) => {
                    log::warn!("[USN] Custom MFT enum failed ({}), falling back to usn-journal-rs", e);
                    let enum_options = EnumOptions {
                        buffer_size: 1024 * 1024,
                        ..Default::default()
                    };
                    raw_entries = Vec::with_capacity(1_000_000);
                    parent_map = HashMap::with_capacity(1_000_000);
                    for result in mft.iter_with_options(enum_options) {
                        let entry = match result { Ok(e) => e, Err(_) => continue };
                        parent_map.insert(entry.fid, entry.parent_fid);
                        raw_entries.push(RawEntry {
                            fid: entry.fid,
                            file_name: entry.file_name,
                            file_attributes: entry.file_attributes,
                            timestamp: 0, // 回退路径无 timestamp
                        });
                    }
                }
            }
        }
        None => {
            log::warn!("[USN] Failed to open volume handle, falling back to usn-journal-rs");
            let enum_options = EnumOptions {
                buffer_size: 1024 * 1024,
                ..Default::default()
            };
            raw_entries = Vec::with_capacity(1_000_000);
            parent_map = HashMap::with_capacity(1_000_000);
            for result in mft.iter_with_options(enum_options) {
                let entry = match result { Ok(e) => e, Err(_) => continue };
                parent_map.insert(entry.fid, entry.parent_fid);
                raw_entries.push(RawEntry {
                    fid: entry.fid,
                    file_name: entry.file_name,
                    file_attributes: entry.file_attributes,
                    timestamp: 0,
                });
            }
        }
    }

    log::info!(
        "[USN] Phase 1a: Enumerated {} MFT entries for {} in {:?}",
        raw_entries.len(), drive_letter, t0.elapsed()
    );

    // Build O(1) index lookup: fid → raw_entries index (avoids cloning OsStrings)
    let fid_to_idx: HashMap<u64, usize> = raw_entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.fid, i))
        .collect();

    // Phase 1b: 并行路径解析，透传 MFT 阶段获取的 timestamp
    // entries 元组结构：(fid, name, path, is_dir, timestamp)
    let t1 = Instant::now();
    let entries: Vec<(u64, Box<str>, Box<str>, bool, i64)> = raw_entries
        .par_iter()
        .enumerate()
        .filter_map(|(_idx, re)| {
            // Skip $-prefixed NTFS metadata files (cheap pre-check before path resolution)
            if re.file_name.to_string_lossy().starts_with('$') {
                return None;
            }
            // Hidden check
            if !include_hidden_files && (re.file_attributes & 0x02) != 0 {
                return None;
            }
            // System check
            if !include_system_files && (re.file_attributes & 0x04) != 0 {
                return None;
            }

            // Resolve path via parent map — O(depth) with O(1) index lookups
            // Use Vec<&OsString> to borrow from raw_entries, avoiding clones
            let mut parts: Vec<&std::ffi::OsString> = Vec::with_capacity(16);
            parts.push(&re.file_name);
            let mut cur_fid = re.fid;
            for _ in 0..50 {
                match parent_map.get(&cur_fid) {
                    Some(&pfid) if pfid != cur_fid && pfid != 0 => {
                        cur_fid = pfid;
                        match fid_to_idx.get(&pfid) {
                            Some(&parent_idx) => {
                                let parent_name = &raw_entries[parent_idx].file_name;
                                // Skip NTFS virtual entries (. and ..) to avoid paths like C:\.\windows
                                if parent_name.to_string_lossy() == "." || parent_name.to_string_lossy() == ".." {
                                    continue;
                                }
                                parts.push(parent_name);
                            }
                            None => break,
                        }
                    }
                    _ => break,
                }
            }
            parts.reverse();

            // Build path string with pre-allocated capacity
            let estimated_len: usize = parts.iter().map(|p| p.len()).sum::<usize>()
                + parts.len()  // separators
                + 4;            // "X:\" prefix
            let path_str: Box<str> = {
                let mut path = String::with_capacity(estimated_len);
                path.push(drive_letter);
                path.push(':');
                path.push('\\');
                for (i, part) in parts.iter().enumerate() {
                    if i > 0 {
                        path.push('\\');
                    }
                    path.push_str(&part.to_string_lossy());
                }
                path.into()
            };

            // Path component checks
            let skip = path_str.split('\\').any(|comp| {
                let sl = comp.to_lowercase();
                sl == RECYCLE_BIN || (!include_system_files && sl == SYSTEM_VOL_INFO)
            });
            if skip {
                return None;
            }

            // Pre-convert name to Box<str> here so Phase 2 doesn't need name lookup
            let name_str: Box<str> = re.file_name.to_string_lossy().into();
            let is_dir = (re.file_attributes & 0x10) != 0;
            Some((re.fid, name_str, path_str, is_dir, re.timestamp))
        })
        .collect();

    log::info!(
        "[USN] Phase 1b: Resolved {} file paths for {} in {:?}",
        entries.len(), drive_letter, t1.elapsed()
    );

    // Free raw_entries and parent_map/fid_to_idx — no longer needed
    drop(raw_entries);
    drop(parent_map);
    drop(fid_to_idx);

    // Phase 2: 仅查询文件大小（timestamp 已在 Phase 1a 从 MFT 拿到）
    // 优化收益：每个文件节省一次 timestamp 字段的传输，
    // 实际节省效果取决于 NTFS 元数据缓存命中率
    let t2 = Instant::now();
    let path_isdir: Vec<(Box<str>, bool)> = entries
        .iter()
        .map(|(_, _, path, is_dir, _)| (path.clone(), *is_dir))
        .collect();
    let sizes = ntfs_mft::batch_metadata(&path_isdir);
    drop(path_isdir); // 释放临时内存

    let files: Vec<SearchResult> = entries
        .par_iter()
        .enumerate()
        .map(|(i, (fid, name_str, path_str, is_dir, timestamp))| {
            let (size, fallback_modified) = sizes[i];
            // 优先使用 MFT 时间戳，回退到 GetFileAttributesExW 返回的 modified_time
            let modified_time = if *timestamp > 0 { *timestamp } else { fallback_modified };
            SearchResult {
                file_id: *fid,
                name: name_str.clone(),
                path: path_str.clone(),
                size,
                modified_time,
                is_directory: *is_dir,
            }
        })
        .collect();

    log::info!(
        "[USN] Phase 2: Batch size lookup for {} files in {:?}",
        files.len(), t2.elapsed()
    );
    log::info!(
        "[USN] Full scan complete for drive {}: {} files, total {:?}",
        drive_letter, files.len(), scan_start.elapsed()
    );

    // Create or verify USN journal
    let journal = volume.journal();
    let journal_max_size = usn_journal_rs::DEFAULT_JOURNAL_MAX_SIZE;
    let allocation_delta = usn_journal_rs::DEFAULT_JOURNAL_ALLOCATION_DELTA;

    let (journal_id, last_usn) = match journal.query(true) {
        Ok(data) => {
            if data.maximum_size < journal_max_size {
                if let Err(e) = journal.create_or_update(journal_max_size, allocation_delta) {
                    log::warn!("[USN] Failed to resize journal for {}: {}", drive_letter, e);
                }
                if let Ok(new_data) = journal.query(false) {
                    (new_data.journal_id, new_data.next_usn)
                } else {
                    (data.journal_id, data.next_usn)
                }
            } else {
                (data.journal_id, data.next_usn)
            }
        }
        Err(e) => {
            log::warn!("[USN] Failed to query journal for {}: {}", drive_letter, e);
            match journal.create_or_update(journal_max_size, allocation_delta) {
                Ok(()) => match journal.query(false) {
                    Ok(data) => (data.journal_id, data.next_usn),
                    Err(e2) => {
                        let _ = resp_tx.send(UsnResponse::Error {
                            message: format!("Journal create+query failed for {}: {}", drive_letter, e2),
                        });
                        return;
                    }
                },
                Err(e2) => {
                    let _ = resp_tx.send(UsnResponse::Error {
                        message: format!("Journal create failed for {}: {}", drive_letter, e2),
                    });
                    return;
                }
            }
        }
    };

    last_usn_map.insert(drive_letter, last_usn);
    journal_id_map.insert(drive_letter, journal_id);

    let mut state = UsnState::load();
    state.volumes.insert(
        drive_letter.to_string(),
        VolumeState { journal_id, last_usn },
    );
    state.save();

    volumes.insert(drive_letter, volume);

    let _ = resp_tx.send(UsnResponse::FullScanResult {
        drive_letter,
        files,
        last_usn,
        journal_id,
    });
}

fn handle_poll_changes(
    drive_letter: char,
    include_hidden_files: bool,
    include_system_files: bool,
    volumes: &HashMap<char, Volume>,
    last_usn_map: &mut HashMap<char, i64>,
    resp_tx: &Sender<UsnResponse>,
) {
    log::info!("[USN] handle_poll_changes entered for drive {}", drive_letter);
    let volume = match volumes.get(&drive_letter) {
        Some(v) => v,
        None => {
            let _ = resp_tx.send(UsnResponse::Error {
                message: format!("No volume handle for drive {}", drive_letter),
            });
            return;
        }
    };

    let last_usn = match last_usn_map.get(&drive_letter) {
        Some(&usn) => usn,
        None => {
            let _ = resp_tx.send(UsnResponse::Error {
                message: format!("No last USN recorded for drive {}", drive_letter),
            });
            return;
        }
    };

    let journal = volume.journal();

    // 查询当前 journal 状态（仅查询，不创建）
    let journal_data = match journal.query(false) {
        Ok(data) => data,
        Err(e) => {
            log::warn!("[USN] Failed to query journal for {}: {}", drive_letter, e);
            let _ = resp_tx.send(UsnResponse::Error {
                message: format!("Failed to query journal for {}: {}", drive_letter, e),
            });
            return;
        }
    };

    // journal_id 一致性检查：若 journal 被重建（ID 变化），重置 last_usn
    // 避免使用旧 journal 的 last_usn 读取新 journal 导致 ERROR_INVALID_PARAMETER
    let mut state = UsnState::load();
    let stored_journal_id = state
        .volumes
        .get(&drive_letter.to_string())
        .map(|vs| vs.journal_id)
        .unwrap_or(0);

    let effective_last_usn = if stored_journal_id != 0 && stored_journal_id != journal_data.journal_id {
        log::warn!(
            "[USN] Journal ID changed for {}: stored={}, current={}, resetting last_usn",
            drive_letter, stored_journal_id, journal_data.journal_id
        );
        // 重置到 next_usn - 1（跳过历史记录，仅监控后续新增变更）
        let reset_usn = journal_data.next_usn.saturating_sub(1);
        last_usn_map.insert(drive_letter, reset_usn);
        // 同步更新持久化状态中的 journal_id 和 last_usn
        if let Some(vs) = state.volumes.get_mut(&drive_letter.to_string()) {
            vs.journal_id = journal_data.journal_id;
            vs.last_usn = reset_usn;
        } else {
            state.volumes.insert(
                drive_letter.to_string(),
                VolumeState {
                    journal_id: journal_data.journal_id,
                    last_usn: reset_usn,
                },
            );
        }
        state.save();
        reset_usn
    } else {
        last_usn
    };

    // 直接使用 effective_last_usn 作为 start_usn（不 +1）
    //
    // 关键：FSCTL_READ_USN_JOURNAL 的 StartUsn 必须是有效的记录边界
    // - 全量扫描后 last_usn = next_usn（有效的读取起点）
    // - 后续轮询 last_usn = API 返回的 next-start USN（也是有效的记录边界）
    // - last_usn + 1 不是有效记录边界，会导致 ERROR_INVALID_PARAMETER (0x80070057)
    //
    // 钳制到 [lowest_valid_usn, next_usn] 范围内以确保安全
    let start_usn = effective_last_usn
        .max(journal_data.lowest_valid_usn)
        .min(journal_data.next_usn);

    log::info!(
        "[USN] Poll {}: last_usn={}, lowest_valid_usn={}, next_usn={}, journal_id={}, using start={}",
        drive_letter, effective_last_usn, journal_data.lowest_valid_usn,
        journal_data.next_usn, journal_data.journal_id, start_usn
    );

    // 若 start_usn >= next_usn，说明无新变更，直接返回空结果
    if start_usn >= journal_data.next_usn {
        log::info!(
            "[USN] Poll {}: start_usn={} >= next_usn={}, no new changes, skipping",
            drive_letter, start_usn, journal_data.next_usn
        );
        let _ = resp_tx.send(UsnResponse::IncrementalResult {
            drive_letter,
            added: Vec::new(),
            removed: Vec::new(),
            updated: Vec::new(),
            last_usn: effective_last_usn,
        });
        return;
    }

    // 直接调用 FSCTL_READ_USN_JOURNAL 读取变更记录
    // 绕过 usn-journal-rs 的 iter_with_options，使用 API 返回的 next-start USN 作为 last_usn
    let vol_handle = match ntfs_mft::open_volume_handle(drive_letter) {
        Some(h) => h,
        None => {
            let _ = resp_tx.send(UsnResponse::Error {
                message: format!("Failed to open volume handle for drive {}", drive_letter),
            });
            return;
        }
    };

    log::info!(
        "[USN] Poll {}: calling FSCTL_READ_USN_JOURNAL directly from start_usn={}",
        drive_letter, start_usn
    );

    let (records, next_start_usn) = match read_usn_records_direct(
        vol_handle,
        journal_data.journal_id,
        start_usn,
    ) {
        Ok(r) => r,
        Err(e) => {
            log::warn!("[USN] Poll {}: read_usn_records_direct error: {}", drive_letter, e);
            let _ = resp_tx.send(UsnResponse::Error {
                message: format!("Failed to read journal for {}: {}", drive_letter, e),
            });
            return;
        }
    };

    let mut resolver = volume.path_resolver_with_cache();
    let mut added: Vec<SearchResult> = Vec::new();
    let mut removed: Vec<u64> = Vec::new();
    let mut updated: Vec<(u64, SearchResult)> = Vec::new();
    // new_last_usn 使用 API 返回的 next_start_usn，这是有效的记录边界
    let new_last_usn = next_start_usn;

    const USN_REASON_FILE_CREATE: u32 = 0x100;
    const USN_REASON_FILE_DELETE: u32 = 0x200;
    const USN_REASON_RENAME_OLD_NAME: u32 = 0x1000;
    const USN_REASON_RENAME_NEW_NAME: u32 = 0x2000;
    const USN_REASON_DATA_OVERWRITE: u32 = 0x01;
    const USN_REASON_BASIC_INFO_CHANGE: u32 = 0x8000;

    const RECYCLE_BIN: &str = "$recycle.bin";
    const SYSTEM_VOL_INFO: &str = "system volume information";

    // Build a parent map from this batch of USN records for fallback path resolution.
    // When usn-journal-rs's resolver can't find a parent (e.g. newly created files),
    // we can walk the parent chain using this map.
    let mut batch_parent_map: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
    let mut batch_name_map: std::collections::HashMap<u64, &std::ffi::OsString> = std::collections::HashMap::new();
    for rec in &records {
        batch_parent_map.insert(rec.fid, rec.parent_fid);
        batch_name_map.insert(rec.fid, &rec.file_name);
    }

    // 复用 volume handle 用于元数据查询
    let mut mft_reader = ntfs_mft::open_volume_handle(drive_letter)
        .map(|handle| ntfs_mft::UsnMetadataReader::new(handle));

    let entry_count = records.len();

    // Dedup: same fid can appear in multiple USN records (create + write + close).
    // Track which fids have already been added/removed/updated in this batch.
    let mut added_fids: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut removed_fids: std::collections::HashSet<u64> = std::collections::HashSet::new();

    for entry in &records {
        let reason = entry.reason;
        let fid = entry.fid;

        // Handle deletion/renamed-away: mark old fid as removed
        if reason & USN_REASON_FILE_DELETE != 0 || reason & USN_REASON_RENAME_OLD_NAME != 0 {
            if removed_fids.insert(fid) {
                removed.push(fid);
            }
            // Clear added_fids so that a subsequent RENAME_NEW_NAME for the same
            // fid (same file, new name) can re-add it. Without this, a new file
            // that is created+renamed in the same batch would be skipped by dedup.
            added_fids.remove(&fid);
        }

        // Handle creation/renamed-to: add new entry
        // NOTE: use independent `if` (not `else if`) so rename records with both
        // RENAME_OLD_NAME + RENAME_NEW_NAME flags process both branches.
        if reason & USN_REASON_FILE_CREATE != 0 || reason & USN_REASON_RENAME_NEW_NAME != 0 {
            // Skip if this fid was already added in this batch (same file can have
            // multiple USN records: create + write + close)
            if !added_fids.insert(fid) {
                continue;
            }
            let path = match resolver.resolve_path(entry) {
                Some(p) => p,
                None => {
                    // Fallback: build path from batch parent map
                    match resolve_path_from_batch(fid, &batch_parent_map, &batch_name_map, drive_letter) {
                        Some(p) => p,
                        None => {
                            log::warn!(
                                "[USN] resolve_path FAILED for fid={}, name={}, reason=0x{:x}, is_dir={}",
                                fid,
                                entry.file_name.to_string_lossy(),
                                reason,
                                entry.is_dir()
                            );
                            continue;
                        }
                    }
                }
            };

            let skip = path.components().any(|comp| {
                let s = comp.as_os_str().to_string_lossy();
                let sl = s.to_lowercase();
                if sl == RECYCLE_BIN { return true; }
                if !include_system_files && sl == SYSTEM_VOL_INFO { return true; }
                false
            });
            if skip { continue; }

            if !include_hidden_files && entry.is_hidden() { continue; }
            if entry.file_name.to_string_lossy().starts_with('$') { continue; }

            let name: Box<str> = Box::from(entry.file_name.to_string_lossy().as_ref());
            let path_str: Box<str> = path.to_string_lossy().to_string().into();
            let meta = mft_reader
                .as_mut()
                .map(|r| r.get_file_metadata(fid, &path))
                .unwrap_or(ntfs_mft::FileMetadata {
                    size: 0,
                    modified_time: 0,
                    is_directory: entry.is_dir(),
                });
            added.push(SearchResult {
                file_id: fid,
                name,
                path: path_str,
                size: meta.size,
                modified_time: meta.modified_time,
                is_directory: meta.is_directory,
            });
        }

        // Handle data/info change
        if reason & USN_REASON_DATA_OVERWRITE != 0
            || reason & USN_REASON_BASIC_INFO_CHANGE != 0
        {
            let path = match resolver.resolve_path(entry) {
                Some(p) => p,
                None => {
                    // Fallback: build path from batch parent map
                    match resolve_path_from_batch(fid, &batch_parent_map, &batch_name_map, drive_letter) {
                        Some(p) => p,
                        None => {
                            log::warn!(
                                "[USN] resolve_path FAILED (update) for fid={}, name={}, reason=0x{:x}",
                                fid,
                                entry.file_name.to_string_lossy(),
                                reason
                            );
                            continue;
                        }
                    }
                }
            };

            let name: Box<str> = Box::from(entry.file_name.to_string_lossy().as_ref());
            let path_str: Box<str> = path.to_string_lossy().to_string().into();
            let meta = mft_reader
                .as_mut()
                .map(|r| r.get_file_metadata(fid, &path))
                .unwrap_or(ntfs_mft::FileMetadata {
                    size: 0,
                    modified_time: 0,
                    is_directory: entry.is_dir(),
                });
            updated.push((fid, SearchResult {
                file_id: fid,
                name,
                path: path_str,
                size: meta.size,
                modified_time: meta.modified_time,
                is_directory: meta.is_directory,
            }));
        }
    }

    log::info!(
        "[USN] Poll {}: entries_read={}, added={}, removed={}, updated={}, new_last_usn={}",
        drive_letter,
        entry_count,
        added.len(),
        removed.len(),
        updated.len(),
        new_last_usn
    );

    // 仅当 new_last_usn 有进展时更新持久化状态
    // 注意：journal_id 变化时 state 已在上方保存，此处仅在 last_usn 推进时更新
    if new_last_usn > effective_last_usn {
        last_usn_map.insert(drive_letter, new_last_usn);

        // 重新加载 state 以避免覆盖其他卷的并发更新
        let mut state = UsnState::load();
        if let Some(vs) = state.volumes.get_mut(&drive_letter.to_string()) {
            vs.last_usn = new_last_usn;
        } else {
            // 首次轮询时持久化状态可能不存在，补建一条
            state.volumes.insert(
                drive_letter.to_string(),
                VolumeState {
                    journal_id: journal_data.journal_id,
                    last_usn: new_last_usn,
                },
            );
        }
        state.save();
    }

    let _ = resp_tx.send(UsnResponse::IncrementalResult {
        drive_letter,
        added,
        removed,
        updated,
        last_usn: new_last_usn,
    });
}
